// SPDX-License-Identifier: MPL-2.0
//! Sound cues: randomised containers with variation rules (`docs/specs/16-audio.md` § 2).
//!
//! A cue picks one of its entries per play, honouring: weights, random pitch/volume ranges,
//! no-repeat-of-the-last-N, sequential/shuffle/random modes, a cooldown, and a max-concurrent
//! limit with a steal policy. Selection is a pure state machine ([`CueSelector`]) so every rule is
//! unit-testable without a device.

use std::collections::VecDeque;
use std::time::Duration;

use regia_core::math::Rng;
use serde::{Deserialize, Serialize};

use crate::error::AudioError;
use crate::sound::{PlaybackParams, SoundAsset};

/// The `.cue.ron` format version. v1 carried **no** `version` field (see
/// `docs/formats/audio-cue-v1.md`); a file without one is v1. v2 (ADR-272) adds the optional
/// `version`, `filter_cutoff_hz` and `filter_offset_octaves` fields; every v1 file loads
/// unchanged because each new field defaults to the old behavior. v3 (ADR-1697) adds the optional
/// `caption` (a sound caption the cue sends once per play); v1 and v2 files load unchanged.
pub const CUE_VERSION: u32 = 3;

/// How a cue walks its entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CuePlayMode {
    /// Weighted random pick among the allowed entries.
    #[default]
    Random,
    /// A shuffle bag: every entry plays once per bag cycle, order randomised.
    Shuffle,
    /// Round-robin, wrapping.
    Sequential,
}

/// What to do when a cue is at its instance limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum StealPolicy {
    /// Stop the oldest instance of this cue and start the new one.
    #[default]
    Oldest,
    /// Stop the quietest instance of this cue and start the new one.
    Quietest,
    /// Refuse the new play.
    None,
}

/// One entry of a cue: the sound and its pick weight.
#[derive(Clone, Debug, PartialEq)]
pub struct CueEntry {
    /// The sound.
    pub asset: SoundAsset,
    /// Relative pick probability (weights need not sum to 1).
    pub weight: f32,
}

/// A randomised container of sounds (`docs/specs/16-audio.md` § 2). Build one with
/// [`SoundCue::new`] in code, or load the RON form ([`SoundCueDef`]) and resolve paths.
///
/// # Example
///
/// ```
/// use regia_audio::{CuePlayMode, SoundCue};
///
/// let cue = SoundCue::new("footsteps", vec![]);
/// assert_eq!(cue.mode, CuePlayMode::Random);
/// assert_eq!(cue.pitch_range, (1.0, 1.0)); // no pitch variation by default
/// assert_eq!(cue.cooldown, std::time::Duration::ZERO);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct SoundCue {
    /// Cue name (used in logs and instance tracking).
    pub name: String,
    /// Entries to pick from.
    pub entries: Vec<CueEntry>,
    /// Pick mode.
    pub mode: CuePlayMode,
    /// Never repeat any of the last N picks (0 = repeats allowed).
    pub no_repeat_last: usize,
    /// Random pitch ratio range `[min, max]` (1 = original).
    pub pitch_range: (f32, f32),
    /// Random volume range in dB `[min, max]`.
    pub volume_range_db: (f32, f32),
    /// At most this many instances of the cue at once; `None` = unlimited.
    pub max_concurrent: Option<usize>,
    /// Steal policy at the limit.
    pub steal: StealPolicy,
    /// Minimum time between plays of this cue.
    pub cooldown: Duration,
    /// Base volume of every pick (before `volume_range_db`).
    pub volume_db: f32,
    /// Bus every pick plays on (`None` = Master).
    pub bus: Option<String>,
    /// Base low-pass cutoff in Hz for every pick; `None` = no filter (unless an offset is set).
    pub filter_cutoff_hz: Option<f32>,
    /// Offset applied to the low-pass cutoff, in octaves (positive opens, negative darkens).
    pub filter_offset_octaves: f32,
    /// The sound caption a play of this cue sends (v3); `None` = silent text.
    pub caption: Option<CueCaptionDef>,
}

/// What kind of caption a cue sends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CueCaptionKind {
    /// A non-speech sound caption (`[door slams]`).
    #[default]
    Sound,
    /// A spoken line.
    Speech,
    /// Narration.
    Narration,
}

/// The caption a cue sends once per play (`caption` in a v3 `.cue.ron`). The text is a Fluent key:
/// `regia_audio` cannot resolve text, so the caption board resolves it when it accepts the message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CueCaptionDef {
    /// The Fluent key of the caption text.
    pub key: String,
    /// The caption kind (default `Sound`).
    #[serde(default)]
    pub kind: CueCaptionKind,
    /// Priority 0 to 255 (default 0).
    #[serde(default)]
    pub priority: u8,
    /// Planned length in milliseconds; absent = the picked sound's length.
    #[serde(default)]
    pub planned_ms: Option<u32>,
}

/// One cue play that sends a caption, drained with `Audio::take_cue_captions`.
#[derive(Clone, Debug, PartialEq)]
pub struct CueCaptionPlay {
    /// The cue name.
    pub cue: String,
    /// The caption to send.
    pub caption: CueCaptionDef,
    /// The voice the play started (the caption ends when it does).
    pub instance: crate::voices::SoundInstanceId,
    /// The picked sound's length in seconds (NaN when unknown).
    pub duration_secs: f64,
}

impl SoundCue {
    /// A cue named `name` over `entries` with default rules (random, no weight bias, no
    /// variation, no limit, no cooldown).
    pub fn new(name: impl Into<String>, entries: Vec<CueEntry>) -> Self {
        Self {
            name: name.into(),
            entries,
            mode: CuePlayMode::Random,
            no_repeat_last: 0,
            pitch_range: (1.0, 1.0),
            volume_range_db: (0.0, 0.0),
            max_concurrent: None,
            steal: StealPolicy::Oldest,
            cooldown: Duration::ZERO,
            volume_db: 0.0,
            bus: None,
            filter_cutoff_hz: None,
            filter_offset_octaves: 0.0,
            caption: None,
        }
    }

    /// The low-pass cutoff (Hz) a pick of this cue plays through, or `None` when the cue has no
    /// filter (no base cutoff and a zero offset: today's unfiltered path, bit-for-bit).
    ///
    /// The cutoff is `base * 2^offset`, clamped to `[20, 20000]` Hz. An offset without a base
    /// cutoff starts from fully open (20 kHz).
    pub fn effective_filter_cutoff_hz(&self) -> Option<f32> {
        if self.filter_cutoff_hz.is_none() && self.filter_offset_octaves == 0.0 {
            return None;
        }
        let base = self.filter_cutoff_hz.unwrap_or(FILTER_OPEN_HZ);
        let hz = base * self.filter_offset_octaves.exp2();
        Some(if hz.is_finite() {
            hz.clamp(FILTER_MIN_HZ, FILTER_OPEN_HZ)
        } else {
            FILTER_OPEN_HZ
        })
    }

    /// The RON-facing definition of a cue (paths, not loaded assets).
    pub fn definition(&self) -> SoundCueDef {
        SoundCueDef {
            version: CUE_VERSION,
            name: self.name.clone(),
            bus: self.bus.clone(),
            sounds: self
                .entries
                .iter()
                .map(|e| CueSoundDef {
                    sound: e.asset.label().to_string(),
                    weight: e.weight,
                })
                .collect(),
            mode: self.mode,
            no_repeat_last: self.no_repeat_last,
            pitch_min: self.pitch_range.0,
            pitch_max: self.pitch_range.1,
            volume_db_min: self.volume_range_db.0,
            volume_db_max: self.volume_range_db.1,
            max_concurrent: self.max_concurrent,
            steal: self.steal,
            cooldown: self.cooldown,
            volume_db: self.volume_db,
            filter_cutoff_hz: self.filter_cutoff_hz,
            filter_offset_octaves: self.filter_offset_octaves,
            caption: self.caption.clone(),
        }
    }
}

/// RON form of a cue (`.cue.ron`): sound paths plus rules.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SoundCueDef {
    /// Format version; absent in v1 files (read as 1). Newer than [`CUE_VERSION`] is a load error.
    #[serde(default = "v1")]
    pub version: u32,
    /// Cue name.
    pub name: String,
    /// Bus every pick plays on.
    #[serde(default)]
    pub bus: Option<String>,
    /// The variations.
    pub sounds: Vec<CueSoundDef>,
    /// Pick mode.
    #[serde(default)]
    pub mode: CuePlayMode,
    /// Never repeat the last N picks.
    #[serde(default)]
    pub no_repeat_last: usize,
    /// Minimum random pitch ratio.
    #[serde(default = "one")]
    pub pitch_min: f32,
    /// Maximum random pitch ratio.
    #[serde(default = "one")]
    pub pitch_max: f32,
    /// Minimum random volume in dB.
    #[serde(default)]
    pub volume_db_min: f32,
    /// Maximum random volume in dB.
    #[serde(default)]
    pub volume_db_max: f32,
    /// Instance limit.
    #[serde(default)]
    pub max_concurrent: Option<usize>,
    /// Steal policy at the limit.
    #[serde(default)]
    pub steal: StealPolicy,
    /// Minimum time between plays.
    #[serde(default)]
    pub cooldown: Duration,
    /// Base volume of every pick.
    #[serde(default)]
    pub volume_db: f32,
    /// Base low-pass cutoff in Hz; absent = no filter (the pre-2a behavior).
    #[serde(default)]
    pub filter_cutoff_hz: Option<f32>,
    /// Octaves added to the low-pass cutoff; default 0 (no offset).
    #[serde(default)]
    pub filter_offset_octaves: f32,
    /// The sound caption a play sends (v3); absent = none.
    #[serde(default)]
    pub caption: Option<CueCaptionDef>,
}

/// Lowest low-pass cutoff a cue may request (Hz).
const FILTER_MIN_HZ: f32 = 20.0;
/// Fully open low-pass (Hz), also the starting point of an offset with no base cutoff.
const FILTER_OPEN_HZ: f32 = 20_000.0;

/// Derives a cue-pick seed from a gameplay event id and the sim tick it fired on. Pure and
/// platform-independent (a SplitMix64 finalizer), so the same event replays the same variation;
/// never reads a clock.
///
/// ```
/// use regia_audio::cue_event_seed;
/// assert_eq!(cue_event_seed(7, 100), cue_event_seed(7, 100));
/// assert_ne!(cue_event_seed(7, 100), cue_event_seed(8, 100));
/// ```
pub fn cue_event_seed(event_id: u64, tick: u64) -> u64 {
    let mut z = event_id
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(tick.wrapping_mul(0xD1B5_4A32_D192_ED03))
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn one() -> f32 {
    1.0
}

fn v1() -> u32 {
    1
}

impl SoundCueDef {
    /// Parses a cue definition from RON text (`.cue.ron`).
    pub fn from_ron(text: &str) -> Result<Self, AudioError> {
        // Version first: a newer file may not parse as this engine's struct, and the useful
        // error is "upgrade the engine", not a RON field error.
        let version = Self::peek_version(text)?;
        if version > CUE_VERSION {
            return Err(AudioError::load(
                "cue",
                format!(
                    "cue format version {version} is newer than this engine ({CUE_VERSION}); upgrade the engine"
                ),
            ));
        }
        ron::from_str(text).map_err(|e| AudioError::load("cue", e))
    }

    /// Serializes the definition to pretty RON (`.cue.ron`).
    pub fn to_ron(&self) -> Result<String, AudioError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty()).map_err(|e| AudioError::load("cue", e))
    }

    /// Migrates v1 cue text to the current version. Every v2 field defaults to the v1 behavior,
    /// so this is load, stamp the version, re-save.
    pub fn migrate_ron(text: &str) -> Result<String, AudioError> {
        let mut def = Self::from_ron(text)?;
        def.version = CUE_VERSION;
        def.to_ron()
    }

    /// The version of cue text without a full parse (1 when the file has no `version`).
    pub fn peek_version(text: &str) -> Result<u32, AudioError> {
        #[derive(Deserialize)]
        struct Peek {
            #[serde(default = "v1")]
            version: u32,
        }
        ron::from_str::<Peek>(text)
            .map(|p| p.version)
            .map_err(|e| AudioError::load("cue", e))
    }

    /// Builds the runtime cue, loading every referenced sound with `resolve` (the asset
    /// server's sound loader).
    pub fn into_cue(
        self,
        resolve: impl Fn(&str) -> Result<SoundAsset, AudioError>,
    ) -> Result<SoundCue, AudioError> {
        if self.sounds.is_empty() {
            return Err(AudioError::Invalid(format!("cue `{}` has no sounds", self.name)));
        }
        let mut entries = Vec::with_capacity(self.sounds.len());
        for sound in &self.sounds {
            entries.push(CueEntry {
                asset: resolve(&sound.sound)?,
                weight: sound.weight,
            });
        }
        Ok(SoundCue {
            name: self.name,
            entries,
            mode: self.mode,
            no_repeat_last: self.no_repeat_last,
            pitch_range: (self.pitch_min, self.pitch_max),
            volume_range_db: (self.volume_db_min, self.volume_db_max),
            max_concurrent: self.max_concurrent,
            steal: self.steal,
            cooldown: self.cooldown,
            volume_db: self.volume_db,
            bus: self.bus,
            filter_cutoff_hz: self.filter_cutoff_hz,
            filter_offset_octaves: self.filter_offset_octaves,
            caption: self.caption,
        })
    }
}

/// One sound reference inside a [`SoundCueDef`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CueSoundDef {
    /// Path of the sound file.
    pub sound: String,
    /// Pick weight.
    #[serde(default = "one")]
    pub weight: f32,
}

/// The serde field-list fingerprint of the `.cue.ron` format (hashed by `cargo xtask check-formats`).
///
/// The root struct *and* its nested sound-entry struct are listed.
pub fn cue_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<SoundCueDef>(),
        regia_core::schema::serde_field_names::<CueSoundDef>(),
        regia_core::schema::serde_field_names::<CueCaptionDef>(),
        regia_core::schema::serde_variant_names::<CuePlayMode>(),
        regia_core::schema::serde_variant_names::<StealPolicy>(),
        regia_core::schema::serde_variant_names::<CueCaptionKind>(),
    ])
}

/// The versions a built-in migration step leaves (`from` side): v1 and v2 → v3
/// ([`SoundCueDef::migrate_ron`]; v3 adds only the optional `caption`).
pub fn cue_migration_sources() -> Vec<u32> {
    vec![1, 2]
}

/// Pure selection state of one cue: enforces mode, no-repeat and variation ranges.
#[derive(Clone)]
pub struct CueSelector {
    mode: CuePlayMode,
    weights: Vec<f32>,
    no_repeat_last: usize,
    pitch_range: (f32, f32),
    volume_range_db: (f32, f32),
    last: VecDeque<usize>,
    shuffle_bag: Vec<usize>,
    sequential_next: usize,
}

impl CueSelector {
    /// State for a cue's rules.
    pub fn new(cue: &SoundCue) -> Self {
        Self {
            mode: cue.mode,
            weights: cue.entries.iter().map(|e| e.weight.max(0.0)).collect(),
            no_repeat_last: cue.no_repeat_last,
            pitch_range: cue.pitch_range,
            volume_range_db: cue.volume_range_db,
            last: VecDeque::new(),
            shuffle_bag: Vec::new(),
            sequential_next: 0,
        }
    }

    /// Picks the next entry index honouring the mode and the no-repeat window.
    pub fn pick(&mut self, rng: &mut Rng) -> usize {
        let len = self.weights.len();
        let index = match self.mode {
            CuePlayMode::Random => self.pick_weighted(rng, &self.allowed()),
            CuePlayMode::Shuffle => {
                if self.shuffle_bag.is_empty() {
                    self.refill_bag(rng);
                }
                // The bag may start with a forbidden entry after a refill; draw until allowed
                // (bounded: at least one entry is always allowed).
                loop {
                    let candidate = self.shuffle_bag.pop().unwrap_or(0);
                    if self.is_allowed(candidate) || self.shuffle_bag.is_empty() {
                        break candidate;
                    }
                    self.shuffle_bag.insert(0, candidate);
                }
            }
            CuePlayMode::Sequential => {
                let mut index = self.sequential_next;
                while !self.is_allowed(index) {
                    index = (index + 1) % len.max(1);
                }
                self.sequential_next = (index + 1) % len.max(1);
                index
            }
        };
        self.remember(index);
        index
    }

    /// Rolls the variation (pitch/volume) of one pick into playback `params`.
    pub fn vary(&self, params: &mut PlaybackParams, rng: &mut Rng) {
        let (p_min, p_max) = self.pitch_range;
        if p_max > p_min {
            params.pitch = p_min + rng.f32() * (p_max - p_min);
        } else if p_max == p_min && p_min.is_finite() && p_min != 1.0 {
            // A locked range pins the pitch; the neutral (1, 1) default leaves the caller's.
            params.pitch = p_min;
        }
        let (v_min, v_max) = self.volume_range_db;
        if v_max >= v_min && v_min.is_finite() && v_max.is_finite() {
            params.volume_db += v_min + rng.f32() * (v_max - v_min);
        }
    }

    fn allowed(&self) -> Vec<usize> {
        (0..self.weights.len()).filter(|&i| self.is_allowed(i)).collect()
    }

    fn is_allowed(&self, index: usize) -> bool {
        !self.last.contains(&index)
    }

    fn pick_weighted(&mut self, rng: &mut Rng, allowed: &[usize]) -> usize {
        if allowed.is_empty() {
            return 0;
        }
        if allowed.len() == 1 {
            return allowed[0];
        }
        let total: f32 = allowed.iter().map(|&i| self.weights[i]).sum();
        if total <= 0.0 {
            return allowed[rng.range(0..allowed.len() as u64) as usize];
        }
        let mut pick = rng.f32() * total;
        for &i in allowed {
            pick -= self.weights[i];
            if pick <= 0.0 {
                return i;
            }
        }
        allowed[allowed.len() - 1]
    }

    fn refill_bag(&mut self, rng: &mut Rng) {
        let len = self.weights.len();
        let mut bag: Vec<usize> = (0..len).collect();
        // Fisher-Yates with the engine's PCG.
        for i in (1..len).rev() {
            let j = rng.range(0..(i + 1) as u64) as usize;
            bag.swap(i, j);
        }
        self.shuffle_bag = bag;
    }

    fn remember(&mut self, index: usize) {
        if self.no_repeat_last == 0 {
            return;
        }
        self.last.push_back(index);
        while self.last.len() > self.no_repeat_last.min(self.weights.len().saturating_sub(1)) {
            self.last.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::SoundClip;

    fn entry(weight: f32) -> CueEntry {
        let clip = SoundClip::new(vec![0.2; 8], 48_000).expect("clip");
        CueEntry {
            asset: SoundAsset::from_clip(&clip),
            weight,
        }
    }

    fn five_variation_cue(no_repeat: usize, mode: CuePlayMode) -> SoundCue {
        let mut cue = SoundCue::new("test", (0..5).map(|_| entry(1.0)).collect());
        cue.no_repeat_last = no_repeat;
        cue.mode = mode;
        cue
    }

    #[test]
    fn cue_with_five_variations_never_repeats_the_last_two() {
        let cue = five_variation_cue(2, CuePlayMode::Random);
        let mut selector = CueSelector::new(&cue);
        let mut rng = Rng::new(0xC0FFEE);
        let mut seen = [0usize; 5];
        let mut history: Vec<usize> = Vec::new();
        for _ in 0..2_000 {
            let pick = selector.pick(&mut rng);
            if history.len() >= 2 {
                let (a, b) = (history[history.len() - 1], history[history.len() - 2]);
                assert_ne!(pick, a, "repeated the immediately previous pick");
                assert_ne!(pick, b, "repeated a pick two back");
            }
            seen[pick] += 1;
            history.push(pick);
        }
        for (i, count) in seen.iter().enumerate() {
            assert!(*count > 50, "variation {i} barely ever plays ({count})");
        }
    }

    #[test]
    fn shuffle_visits_every_entry_per_bag_cycle() {
        let cue = five_variation_cue(2, CuePlayMode::Shuffle);
        let mut selector = CueSelector::new(&cue);
        let mut rng = Rng::new(7);
        for _ in 0..50 {
            let mut cycle = Vec::new();
            for _ in 0..5 {
                cycle.push(selector.pick(&mut rng));
            }
            let mut sorted = cycle.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, vec![0, 1, 2, 3, 4], "a bag cycle must cover all entries");
        }
    }

    #[test]
    fn sequential_cycles_in_order() {
        let cue = five_variation_cue(2, CuePlayMode::Sequential);
        let mut selector = CueSelector::new(&cue);
        let mut rng = Rng::new(1);
        let picks: Vec<usize> = (0..10).map(|_| selector.pick(&mut rng)).collect();
        assert_eq!(picks, vec![0, 1, 2, 3, 4, 0, 1, 2, 3, 4]);
    }

    #[test]
    fn weighted_random_prefers_heavy_entries() {
        let mut cue = SoundCue::new("weighted", vec![entry(0.01), entry(100.0)]);
        cue.no_repeat_last = 0;
        let mut selector = CueSelector::new(&cue);
        let mut rng = Rng::new(42);
        let mut heavy = 0;
        for _ in 0..1_000 {
            if selector.pick(&mut rng) == 1 {
                heavy += 1;
            }
        }
        assert!(heavy > 900, "heavy entry picked {heavy}/1000 times");
    }

    #[test]
    fn variation_ranges_are_applied() {
        let mut cue = five_variation_cue(0, CuePlayMode::Random);
        cue.pitch_range = (0.9, 1.1);
        cue.volume_range_db = (-6.0, 0.0);
        let selector = CueSelector::new(&cue);
        let mut rng = Rng::new(3);
        let (mut pitch_lo, mut pitch_hi) = (f32::MAX, f32::MIN);
        let (mut vol_lo, mut vol_hi) = (f32::MAX, f32::MIN);
        for _ in 0..100 {
            let mut params = PlaybackParams::default();
            selector.vary(&mut params, &mut rng);
            assert!((0.9..=1.1).contains(&params.pitch));
            assert!((-6.0..=0.0).contains(&params.volume_db));
            pitch_lo = pitch_lo.min(params.pitch);
            pitch_hi = pitch_hi.max(params.pitch);
            vol_lo = vol_lo.min(params.volume_db);
            vol_hi = vol_hi.max(params.volume_db);
        }
        // The rolls actually spread over both ranges (not stuck at the defaults 1.0 and 0 dB).
        assert!(pitch_lo < 0.95 && pitch_hi > 1.05, "pitch {pitch_lo}..{pitch_hi}");
        assert!(vol_lo < -4.5 && vol_hi < 0.0, "volume {vol_lo}..{vol_hi}");
        assert!(vol_hi > -1.5, "volume reaches the quiet-loss end {vol_hi}");
    }

    #[test]
    fn equal_pitch_and_volume_ranges_lock_the_value() {
        let mut cue = five_variation_cue(0, CuePlayMode::Random);
        cue.pitch_range = (0.5, 0.5);
        cue.volume_range_db = (-6.0, -6.0);
        let selector = CueSelector::new(&cue);
        let mut rng = Rng::new(3);
        let mut params = PlaybackParams::default();
        selector.vary(&mut params, &mut rng);
        assert_eq!(params.pitch, 0.5);
        assert_eq!(params.volume_db, -6.0);
        // The neutral default range leaves the caller's pitch alone.
        let neutral = CueSelector::new(&five_variation_cue(0, CuePlayMode::Random));
        let mut params = PlaybackParams {
            pitch: 1.25,
            ..PlaybackParams::default()
        };
        neutral.vary(&mut params, &mut rng);
        assert_eq!(params.pitch, 1.25);
    }

    #[test]
    fn no_repeat_window_never_starves_the_picker() {
        // 5 entries, window clamps to len-1 = 4: always at least one candidate.
        let cue = five_variation_cue(9, CuePlayMode::Random);
        let mut selector = CueSelector::new(&cue);
        let mut rng = Rng::new(11);
        let mut recent: Vec<usize> = Vec::new();
        for _ in 0..100 {
            let pick = selector.pick(&mut rng);
            assert!(pick < 5);
            assert!(!recent.contains(&pick), "{pick} repeated within {recent:?}");
            recent.push(pick);
            if recent.len() > 4 {
                recent.remove(0);
            }
        }
    }

    #[test]
    fn old_cue_ron_loads_with_no_filter() {
        let def = SoundCueDef::from_ron(r#"(name: "old", sounds: [(sound: "a.ogg")], mode: Random)"#)
            .expect("old shape parses");
        assert_eq!(def.version, 1);
        assert_eq!(def.filter_cutoff_hz, None);
        assert_eq!(def.filter_offset_octaves, 0.0);
        let cue = SoundCue::new("old", vec![entry(1.0)]);
        assert_eq!(cue.effective_filter_cutoff_hz(), None);
    }

    #[test]
    fn filter_offset_scales_cutoff_in_octaves_and_clamps() {
        let mut cue = SoundCue::new("f", vec![entry(1.0)]);
        cue.filter_cutoff_hz = Some(4_000.0);
        assert_eq!(cue.effective_filter_cutoff_hz(), Some(4_000.0));
        cue.filter_offset_octaves = -1.0;
        assert_eq!(cue.effective_filter_cutoff_hz(), Some(2_000.0));
        cue.filter_offset_octaves = 10.0;
        assert_eq!(cue.effective_filter_cutoff_hz(), Some(20_000.0));
        cue.filter_offset_octaves = -20.0;
        assert_eq!(cue.effective_filter_cutoff_hz(), Some(20.0));
        // An offset alone starts from fully open.
        cue.filter_cutoff_hz = None;
        cue.filter_offset_octaves = -2.0;
        assert_eq!(cue.effective_filter_cutoff_hz(), Some(5_000.0));
    }

    #[test]
    fn filter_fields_round_trip_through_ron() {
        let text = r#"(name: "f", sounds: [(sound: "a.ogg")],
            filter_cutoff_hz: Some(3000.0), filter_offset_octaves: -0.5)"#;
        let def = SoundCueDef::from_ron(text).expect("parse");
        assert_eq!(def.filter_cutoff_hz, Some(3000.0));
        assert_eq!(def.filter_offset_octaves, -0.5);
        let mut cue = SoundCue::new("f", vec![entry(1.0)]);
        cue.filter_cutoff_hz = def.filter_cutoff_hz;
        cue.filter_offset_octaves = def.filter_offset_octaves;
        assert_eq!(cue.definition().filter_offset_octaves, -0.5);
    }

    #[test]
    fn event_seed_is_deterministic_and_spreads() {
        assert_eq!(cue_event_seed(1, 2), cue_event_seed(1, 2));
        // Pinned: replay determinism depends on these exact values on every platform.
        assert_eq!(cue_event_seed(0, 0), 0xe220_a839_7b1d_cdaf);
        assert_eq!(cue_event_seed(1, 2), 0x5a1e_518f_cfcf_0d64);
        assert_eq!(cue_event_seed(42, 1000), 0xe343_f325_6fa7_f02c);
        let mut seen = std::collections::HashSet::new();
        for id in 0..64u64 {
            for tick in 0..16u64 {
                assert!(seen.insert(cue_event_seed(id, tick)), "collision at {id},{tick}");
            }
        }
    }

    #[test]
    fn same_seed_gives_same_pick_and_variation() {
        let mut cue = five_variation_cue(0, CuePlayMode::Random);
        cue.pitch_range = (0.9, 1.1);
        let run = |seed: u64| {
            let mut selector = CueSelector::new(&cue);
            let mut rng = Rng::with_stream(seed, 1);
            let pick = selector.pick(&mut rng);
            let mut params = PlaybackParams::default();
            selector.vary(&mut params, &mut rng);
            (pick, params.pitch)
        };
        let seed = cue_event_seed(42, 1000);
        assert_eq!(run(seed), run(seed));
        let distinct: std::collections::HashSet<u32> = (0..32)
            .map(|i| run(cue_event_seed(i, 1000)).1.to_bits())
            .collect();
        assert!(distinct.len() > 8, "seeds should vary the pitch");
    }

    #[test]
    fn v1_text_migrates_to_v2_with_unchanged_rules() {
        let old = r#"(name: "old", sounds: [(sound: "a.ogg", weight: 2.0)], no_repeat_last: 1, pitch_min: 0.9, pitch_max: 1.1)"#;
        assert_eq!(SoundCueDef::peek_version(old).expect("peek"), 1);
        let new = SoundCueDef::migrate_ron(old).expect("migrate");
        assert_eq!(SoundCueDef::peek_version(&new).expect("peek"), CUE_VERSION);
        let (a, b) = (
            SoundCueDef::from_ron(old).expect("old"),
            SoundCueDef::from_ron(&new).expect("new"),
        );
        assert_eq!(
            SoundCueDef {
                version: CUE_VERSION,
                ..a
            },
            b
        );
    }

    #[test]
    fn v2_cue_loads_unchanged_and_v3_caption_round_trips() {
        let v2 = r#"(version: 2, name: "x", sounds: [(sound: "a.ogg")])"#;
        let def = SoundCueDef::from_ron(v2).expect("v2 loads");
        assert_eq!(def.version, 2);
        assert!(def.caption.is_none());
        let migrated = SoundCueDef::migrate_ron(v2).expect("migrate");
        assert_eq!(SoundCueDef::peek_version(&migrated).expect("peek"), CUE_VERSION);

        let v3 = r#"(version: 3, name: "x", sounds: [(sound: "a.ogg")],
            caption: Some((key: "cap-x", kind: Sound, priority: 2, planned_ms: Some(900))))"#;
        let def = SoundCueDef::from_ron(v3).expect("v3 loads");
        let caption = def.caption.clone().expect("caption");
        assert_eq!(
            (caption.key.as_str(), caption.priority, caption.planned_ms),
            ("cap-x", 2, Some(900))
        );
        let back = SoundCueDef::from_ron(&def.to_ron().expect("save")).expect("reload");
        assert_eq!(back, def);
        // Defaults: kind Sound, priority 0, no planned length.
        let minimal = SoundCueDef::from_ron(
            r#"(version: 3, name: "x", sounds: [(sound: "a.ogg")], caption: Some((key: "k")))"#,
        )
        .expect("minimal");
        let caption = minimal.caption.expect("caption");
        assert_eq!(
            (caption.kind, caption.priority, caption.planned_ms),
            (CueCaptionKind::Sound, 0, None)
        );
    }

    #[test]
    fn the_fingerprint_names_the_caption_fields() {
        let fingerprint = cue_schema_fingerprint();
        assert!(fingerprint.contains("planned_ms"), "{fingerprint}");
        assert_eq!(cue_migration_sources(), vec![1, 2]);
    }

    #[test]
    fn newer_cue_version_is_rejected() {
        let text = r#"(version: 99, name: "x", sounds: [(sound: "a.ogg")])"#;
        assert!(SoundCueDef::from_ron(text).is_err());
    }

    #[test]
    fn newer_cue_reports_upgrade_even_when_the_body_does_not_parse() {
        let text = r#"(version: 99, name: "x", sounds: [(sound: "a.ogg")], future_field: Bogus(1))"#;
        let error = SoundCueDef::from_ron(text).expect_err("newer").to_string();
        assert!(error.contains("upgrade the engine"), "{error}");
        let v1 = SoundCueDef::from_ron(r#"(name: "x", sounds: [(sound: "a.ogg")])"#).expect("v1");
        assert_eq!(v1.version, 1);
    }
}
