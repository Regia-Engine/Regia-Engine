// SPDX-License-Identifier: MPL-2.0
//! `.ambience.ron` v1 (ADR-2721, `docs/formats/ambience-ron.md`): an ambience as an asset.
//!
//! A file holds looping **beds** (the [`BedLayer`] shape: a sound whose level is a curve of one
//! [`BedDrive`] of the [`AmbienceInputs`]) and random **one-shots**: a list of variant sounds
//! heard at a random interval in a range, from a random place around the listener at a distance
//! in a range, never the same variant twice in a row, optionally levelled by a drive curve of
//! their own (birds that fall silent in the rain). A `seed` makes every interval, variant and
//! place the same on every run: [`AmbiencePlayer`] draws them from one generator seeded by it.
//!
//! Places are uniform over the ring between the two distances (`r = √(u (R² − r₀²) + r₀²)`,
//! the textbook area-uniform disc sample: Pharr, Jakob and Humphreys, "Physically Based
//! Rendering", 4th ed., § A.5.1), at a uniform height in the height range.

use regia_core::math::{Rng, Vec3};
use serde::{Deserialize, Serialize};

use super::bed::{AmbienceBed, AmbienceInputs, BedDrive, BedLayer};
use crate::bus::MUTE_DB;

/// The current `.ambience.ron` format version.
pub const AMBIENCE_VERSION: u32 = 1;

/// Shortest one-shot interval accepted, seconds. Far below this the per-step due-call loop in
/// [`AmbiencePlayer::step`] would run once per due call: a crafted tiny interval in an
/// (untrusted) asset file is an effective infinite loop with unbounded call growth on the game
/// thread, so validation enforces this floor.
pub const MIN_AMBIENCE_INTERVAL_SECS: f32 = 0.01;

/// The most times one one-shot falls due in one `AmbiencePlayer::step` (the engine's 0.25 s step
/// at the interval floor is 25).
const MAX_CALLS_PER_STEP: usize = 256;

/// Longest one-shot interval accepted, seconds.
pub const MAX_AMBIENCE_INTERVAL_SECS: f32 = 3_600.0;

/// Farthest one-shot distance accepted, metres.
pub const MAX_AMBIENCE_DISTANCE_M: f32 = 10_000.0;

/// Why an ambience file was refused; [`AmbienceError::code`] is stable.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum AmbienceError {
    /// Not valid RON for the format.
    #[error("ambience file does not parse: {0}")]
    Parse(String),
    /// A version this engine does not read.
    #[error("ambience format version {0} is not supported (this engine reads version 1); upgrade the engine")]
    Version(u32),
    /// A value out of range or a duplicate name; the message names it.
    #[error("ambience file is invalid: {0}")]
    Invalid(String),
}

impl AmbienceError {
    /// `ambience_parse`, `ambience_version` or `ambience_invalid`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "ambience_parse",
            Self::Version(_) => "ambience_version",
            Self::Invalid(_) => "ambience_invalid",
        }
    }
}

fn default_bus() -> String {
    "Ambience".into()
}

fn default_calm() -> f32 {
    0.3
}

fn default_attack() -> f32 {
    0.4
}

fn default_release() -> f32 {
    1.5
}

/// One random one-shot source of an ambience.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AmbienceOneShot {
    /// Name (unique in the file).
    pub name: String,
    /// The variant sounds, by their [`crate::SoundAssets`] names (at least one).
    pub sounds: Vec<String>,
    /// Seconds between calls, `(min, max)`, drawn uniformly
    /// ([`MIN_AMBIENCE_INTERVAL_SECS`] ≤ min ≤ max).
    pub interval_secs: (f32, f32),
    /// Distance from the listener, metres, `(min, max)` (`0 ≤ min ≤ max`).
    pub distance_m: (f32, f32),
    /// Height above the listener, metres, `(min, max)`.
    #[serde(default)]
    pub height_m: (f32, f32),
    /// Level, dB, `(min, max)`, drawn uniformly per call.
    #[serde(default)]
    pub volume_db: (f32, f32),
    /// What the optional level curve follows.
    #[serde(default)]
    pub drive: BedDrive,
    /// `(drive value, dB offset)` points, linear between and clamped at both ends; empty = no
    /// offset. An offset at [`MUTE_DB`] (−100 dB) or below silences the calls (they are skipped,
    /// the timer keeps running).
    #[serde(default)]
    pub curve: Vec<(f32, f32)>,
}

/// An `.ambience.ron` file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AmbienceDef {
    /// Format version ([`AMBIENCE_VERSION`]).
    pub version: u32,
    /// The ambience's name.
    pub name: String,
    /// Seed of every random draw (intervals, variants, places, levels).
    #[serde(default)]
    pub seed: u64,
    /// The bus beds and one-shots play on.
    #[serde(default = "default_bus")]
    pub bus: String,
    /// Master offset, dB.
    #[serde(default)]
    pub volume_db: f32,
    /// Wind below which the wind drives read 0, m/s.
    #[serde(default = "default_calm")]
    pub calm_m_per_s: f32,
    /// Time constant of a rising bed level, seconds.
    #[serde(default = "default_attack")]
    pub attack_secs: f32,
    /// Time constant of a falling bed level, seconds.
    #[serde(default = "default_release")]
    pub release_secs: f32,
    /// The looping beds.
    #[serde(default)]
    pub beds: Vec<BedLayer>,
    /// The random one-shots.
    #[serde(default)]
    pub one_shots: Vec<AmbienceOneShot>,
}

/// The `.ambience.ron` schema fingerprint (`cargo xtask check-formats`).
pub fn ambience_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<AmbienceDef>(),
        regia_core::schema::serde_field_names::<AmbienceOneShot>(),
        regia_core::schema::serde_field_names::<BedLayer>(),
        regia_core::schema::serde_variant_names::<BedDrive>(),
    ])
}

/// The versions a built-in migration step leaves: none (v1 is the first version).
pub fn ambience_migration_sources() -> Vec<u32> {
    Vec::new()
}

fn finite_range(
    what: &str,
    name: &str,
    (lo, hi): (f32, f32),
    min: f32,
    max: f32,
) -> Result<(), AmbienceError> {
    if !lo.is_finite() || !hi.is_finite() || lo < min || hi > max || lo > hi {
        return Err(AmbienceError::Invalid(format!(
            "`{name}`: {what} ({lo}, {hi}) must be finite with {min} <= min <= max <= {max}"
        )));
    }
    Ok(())
}

fn finite_curve(name: &str, curve: &[(f32, f32)]) -> Result<(), AmbienceError> {
    if curve.iter().any(|(x, y)| !x.is_finite() || !y.is_finite()) {
        return Err(AmbienceError::Invalid(format!(
            "`{name}`: curve points must be finite"
        )));
    }
    Ok(())
}

impl AmbienceDef {
    /// Parses and validates a file.
    ///
    /// ```
    /// # use regia_audio::AmbienceDef;
    /// let def = AmbienceDef::from_ron(r#"( version: 1, name: "glade", seed: 7 )"#).expect("valid");
    /// assert_eq!(def.bus, "Ambience");
    /// assert_eq!(AmbienceDef::from_ron(r#"( version: 2, name: "x" )"#).map_err(|e| e.code()), Err("ambience_version"));
    /// ```
    pub fn from_ron(text: &str) -> Result<Self, AmbienceError> {
        // The version first, so a newer file says "upgrade the engine" instead of a field error.
        #[derive(Deserialize)]
        struct VersionOnly {
            #[serde(default)]
            version: Option<u32>,
        }
        if let Ok(VersionOnly { version: Some(v) }) = ron::from_str::<VersionOnly>(text)
            && v != AMBIENCE_VERSION
        {
            return Err(AmbienceError::Version(v));
        }
        let def: Self = ron::from_str(text).map_err(|e| AmbienceError::Parse(e.to_string()))?;
        def.validate()?;
        Ok(def)
    }

    /// Serializes to pretty RON (round-trips through [`Self::from_ron`]).
    pub fn to_ron(&self) -> Result<String, AmbienceError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| AmbienceError::Parse(e.to_string()))
    }

    /// Checks ranges, names and curves.
    pub fn validate(&self) -> Result<(), AmbienceError> {
        if self.version != AMBIENCE_VERSION {
            return Err(AmbienceError::Version(self.version));
        }
        if self.name.trim().is_empty() {
            return Err(AmbienceError::Invalid("the ambience needs a name".into()));
        }
        for (what, value) in [
            ("volume_db", self.volume_db),
            ("calm_m_per_s", self.calm_m_per_s),
            ("attack_secs", self.attack_secs),
            ("release_secs", self.release_secs),
        ] {
            if !value.is_finite() || (what != "volume_db" && value < 0.0) {
                return Err(AmbienceError::Invalid(format!(
                    "{what} must be finite and non-negative, got {value}"
                )));
            }
        }
        let mut names: Vec<&str> = Vec::new();
        for bed in &self.beds {
            if names.contains(&bed.name.as_str()) {
                return Err(AmbienceError::Invalid(format!("`{}` is defined twice", bed.name)));
            }
            names.push(&bed.name);
            finite_curve(&bed.name, &bed.curve)?;
        }
        for shot in &self.one_shots {
            if names.contains(&shot.name.as_str()) {
                return Err(AmbienceError::Invalid(format!(
                    "`{}` is defined twice",
                    shot.name
                )));
            }
            names.push(&shot.name);
            if shot.sounds.is_empty() {
                return Err(AmbienceError::Invalid(format!("`{}` has no sounds", shot.name)));
            }
            finite_range(
                "interval_secs",
                &shot.name,
                shot.interval_secs,
                MIN_AMBIENCE_INTERVAL_SECS,
                MAX_AMBIENCE_INTERVAL_SECS,
            )?;
            finite_range(
                "distance_m",
                &shot.name,
                shot.distance_m,
                0.0,
                MAX_AMBIENCE_DISTANCE_M,
            )?;
            finite_range(
                "height_m",
                &shot.name,
                shot.height_m,
                -MAX_AMBIENCE_DISTANCE_M,
                MAX_AMBIENCE_DISTANCE_M,
            )?;
            finite_range("volume_db", &shot.name, shot.volume_db, MUTE_DB, 24.0)?;
            finite_curve(&shot.name, &shot.curve)?;
        }
        Ok(())
    }

    /// The bed part as an [`AmbienceBed`] (the levels and slewing of the existing bed).
    #[must_use]
    pub fn bed(&self) -> AmbienceBed {
        let mut bed = AmbienceBed::with_layers(self.beds.clone());
        bed.calm_m_per_s = self.calm_m_per_s;
        bed.attack_secs = self.attack_secs;
        bed.release_secs = self.release_secs;
        bed.bus = self.bus.clone();
        bed.volume_db = self.volume_db;
        bed
    }
}

/// One one-shot call that falls due.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AmbienceCall {
    /// Index of the one-shot in the file.
    pub one_shot: usize,
    /// The variant's sound name.
    pub sound: String,
    /// Where it is heard, world space.
    pub position: Vec3,
    /// Its level, dB (the file's master offset and the drive curve included).
    pub volume_db: f32,
    /// The ambience clock when it fell due, seconds.
    pub at_secs: f64,
}

/// Plays an [`AmbienceDef`] deterministically: the bed levels and the due one-shot calls per
/// step, every random draw from one generator seeded by the file.
#[derive(Clone, Debug)]
pub struct AmbiencePlayer {
    def: AmbienceDef,
    bed: AmbienceBed,
    rng: Rng,
    /// Seconds to the next call of each one-shot.
    timers: Vec<f32>,
    last_variant: Vec<Option<usize>>,
    clock_secs: f64,
    calls: u64,
    skipped: u64,
}

impl AmbiencePlayer {
    /// A player for `def` (validated).
    pub fn new(def: AmbienceDef) -> Result<Self, AmbienceError> {
        def.validate()?;
        let mut rng = Rng::new(def.seed);
        let timers = def
            .one_shots
            .iter()
            .map(|shot| uniform(&mut rng, shot.interval_secs))
            .collect();
        Ok(Self {
            bed: def.bed(),
            last_variant: vec![None; def.one_shots.len()],
            timers,
            rng,
            def,
            clock_secs: 0.0,
            calls: 0,
            skipped: 0,
        })
    }

    /// The file.
    #[must_use]
    pub fn def(&self) -> &AmbienceDef {
        &self.def
    }

    /// The bed state (levels per layer).
    #[must_use]
    pub fn bed(&self) -> &AmbienceBed {
        &self.bed
    }

    /// The ambience clock, seconds.
    #[must_use]
    pub fn clock_secs(&self) -> f64 {
        self.clock_secs
    }

    /// Calls made and calls skipped (silenced by their curve) so far.
    #[must_use]
    pub fn call_counts(&self) -> (u64, u64) {
        (self.calls, self.skipped)
    }

    /// Advances by `dt` seconds with the scene `inputs` and the listener at `listener`: steps the
    /// bed levels and returns the one-shot calls that fell due, in time order. A NaN or negative
    /// step does nothing. Each one-shot falls due at most 256 times per step; a longer backlog
    /// (a host step far past the engine's 0.25 s clamp) is dropped, not replayed.
    pub fn step(&mut self, inputs: &AmbienceInputs, dt: f32, listener: Vec3) -> Vec<AmbienceCall> {
        if !dt.is_finite() || dt <= 0.0 {
            return Vec::new();
        }
        self.bed.step(inputs, dt);
        let start = self.clock_secs;
        self.clock_secs += f64::from(dt);
        let mut calls = Vec::new();
        'shots: for (index, shot) in self.def.one_shots.iter().enumerate() {
            let mut elapsed = 0.0f32;
            let mut due = 0usize;
            // Every call that falls inside this step (a long step can hold several).
            while self.timers[index] <= dt - elapsed {
                if due == MAX_CALLS_PER_STEP {
                    // A host step far past the engine's 0.25 s clamp: drop the backlog (a long
                    // gap is not replayed) and wait one fresh interval from the step's end. This
                    // also ends the loop where an f32 `elapsed` would stop advancing.
                    self.timers[index] = uniform(&mut self.rng, shot.interval_secs);
                    continue 'shots;
                }
                due += 1;
                elapsed += self.timers[index];
                self.timers[index] = uniform(&mut self.rng, shot.interval_secs);
                let variant = super::pick_variant(&mut self.rng, shot.sounds.len(), self.last_variant[index]);
                let angle = self.rng.f32() * std::f32::consts::TAU;
                let (r0, r1) = shot.distance_m;
                let radius = (self.rng.f32() * (r1 * r1 - r0 * r0) + r0 * r0).max(0.0).sqrt();
                let height = uniform(&mut self.rng, shot.height_m);
                let level = uniform(&mut self.rng, shot.volume_db);
                let offset = if shot.curve.is_empty() {
                    0.0
                } else {
                    // Unlike a bed layer, a drive at 0 reads the curve too: birds sing in the dry.
                    let value = shot.drive.value(inputs, self.def.calm_m_per_s);
                    let db = crate::spatial::polyline(&shot.curve, value);
                    if db.is_finite() { db.max(MUTE_DB) } else { MUTE_DB }
                };
                let Some(variant) = variant else { continue };
                if offset <= MUTE_DB {
                    self.skipped += 1;
                    continue;
                }
                self.last_variant[index] = Some(variant);
                self.calls += 1;
                calls.push(AmbienceCall {
                    one_shot: index,
                    sound: shot.sounds[variant].clone(),
                    position: listener + Vec3::new(angle.cos() * radius, height, angle.sin() * radius),
                    volume_db: (level + offset + self.def.volume_db).clamp(MUTE_DB, 24.0),
                    at_secs: start + f64::from(elapsed),
                });
            }
            self.timers[index] -= dt - elapsed;
        }
        calls.sort_by(|a, b| a.at_secs.total_cmp(&b.at_secs).then(a.one_shot.cmp(&b.one_shot)));
        calls
    }
}

/// A uniform draw in `(lo, hi)` (the lower end when they are equal or reversed).
fn uniform(rng: &mut Rng, (lo, hi): (f32, f32)) -> f32 {
    if hi > lo { lo + rng.f32() * (hi - lo) } else { lo }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../../docs/formats/glade.ambience.ron");

    #[test]
    fn the_example_parses_and_round_trips() {
        let def = AmbienceDef::from_ron(EXAMPLE).expect("the docs example is valid");
        assert_eq!(def.version, AMBIENCE_VERSION);
        assert!(!def.beds.is_empty() && !def.one_shots.is_empty());
        let text = def.to_ron().expect("serialize");
        assert_eq!(AmbienceDef::from_ron(&text).expect("reparse"), def);
    }

    #[test]
    fn the_same_seed_draws_the_same_calls() {
        let def = AmbienceDef::from_ron(EXAMPLE).expect("valid");
        let run = |def: &AmbienceDef| {
            let mut player = AmbiencePlayer::new(def.clone()).expect("player");
            let mut calls = Vec::new();
            for _ in 0..3_600 {
                calls.extend(player.step(&AmbienceInputs::default(), 1.0 / 60.0, Vec3::ZERO));
            }
            calls
        };
        let a = run(&def);
        assert!(a.len() > 3, "{} calls in a minute", a.len());
        assert_eq!(a, run(&def));
        let mut other = def.clone();
        other.seed += 1;
        assert_ne!(a, run(&other), "another seed draws other calls");
    }

    #[test]
    fn calls_respect_their_ranges_and_never_repeat_a_variant() {
        let def = AmbienceDef::from_ron(EXAMPLE).expect("valid");
        let mut player = AmbiencePlayer::new(def.clone()).expect("player");
        let listener = Vec3::new(10.0, 1.0, -4.0);
        let mut last: Vec<Option<String>> = vec![None; def.one_shots.len()];
        let mut previous_at = vec![None; def.one_shots.len()];
        for _ in 0..20_000 {
            for call in player.step(&AmbienceInputs::default(), 0.05, listener) {
                let shot = &def.one_shots[call.one_shot];
                let offset = call.position - listener;
                let flat = (offset.x * offset.x + offset.z * offset.z).sqrt();
                assert!(
                    flat >= shot.distance_m.0 - 1e-3 && flat <= shot.distance_m.1 + 1e-3,
                    "{flat}"
                );
                assert!(offset.y >= shot.height_m.0 - 1e-4 && offset.y <= shot.height_m.1 + 1e-4);
                if shot.sounds.len() > 1 {
                    assert_ne!(last[call.one_shot].as_deref(), Some(call.sound.as_str()));
                }
                if let Some(at) = previous_at[call.one_shot] {
                    let gap = call.at_secs - at;
                    assert!(gap >= f64::from(shot.interval_secs.0) - 1e-3, "{gap}");
                    assert!(gap <= f64::from(shot.interval_secs.1) + 1e-3, "{gap}");
                }
                previous_at[call.one_shot] = Some(call.at_secs);
                last[call.one_shot] = Some(call.sound.clone());
            }
        }
    }

    /// Rev-kimi-28 (audio-k finding 4): `AmbiencePlayer::step` is public, and a host may pass
    /// a huge `dt` (the engine clamps its own to 0.25 s). The due-call loop ran `dt / interval`
    /// times per one-shot (a million calls for 10 000 s at the 0.01 s floor), and past about
    /// 1.7e5 s the f32 `elapsed` stopped advancing, so it never ended. A step now makes at most
    /// `MAX_CALLS_PER_STEP` calls per one-shot and drops the backlog beyond it.
    #[test]
    fn a_huge_step_makes_a_bounded_number_of_calls() {
        let text = r#"(
            version: 1, name: "fast", seed: 5,
            one_shots: [( name: "ticks", sounds: ["a", "b"], interval_secs: (0.01, 0.01), distance_m: (5.0, 10.0) )],
        )"#;
        let def = AmbienceDef::from_ron(text).expect("valid");
        let mut player = AmbiencePlayer::new(def).expect("player");
        let calls = player.step(&AmbienceInputs::default(), 10_000.0, Vec3::ZERO);
        assert!(
            !calls.is_empty() && calls.len() <= MAX_CALLS_PER_STEP,
            "{} calls in one step",
            calls.len()
        );
        // The dropped backlog does not come back: the next frame plays at the authored rate.
        let next = player.step(&AmbienceInputs::default(), 1.0 / 60.0, Vec3::ZERO);
        assert!(next.len() <= 2, "{} calls in the next frame", next.len());
    }

    #[test]
    fn a_rain_curve_silences_birds() {
        let text = r#"(
            version: 1, name: "wet", seed: 3,
            one_shots: [( name: "birds", sounds: ["a", "b"], interval_secs: (0.5, 1.0), distance_m: (5.0, 10.0),
                          drive: Rain, curve: [(0.0, 0.0), (1.0, -100.0)] )],
        )"#;
        // In 5 mm/h of rain the curve reads its last point, -100 dB: every call is skipped.
        let def = AmbienceDef::from_ron(text).expect("valid");
        let mut player = AmbiencePlayer::new(def).expect("player");
        let rain = AmbienceInputs {
            rain_mm_per_h: 5.0,
            ..AmbienceInputs::default()
        };
        let mut heard = 0;
        for _ in 0..600 {
            heard += player.step(&rain, 0.1, Vec3::ZERO).len();
        }
        assert_eq!(heard, 0);
        assert!(player.call_counts().1 > 0, "the timer kept running");
    }

    #[test]
    fn a_degenerate_interval_is_refused() {
        // A crafted tiny interval used to pass validation: `AmbiencePlayer::step` then ran its
        // due-call loop about `dt / interval` times per frame (an effective infinite loop with
        // unbounded call growth on the game thread). Asset files are untrusted input, so the
        // floor is enforced here.
        let text = r#"(
            version: 1, name: "x",
            one_shots: [( name: "a", sounds: ["s"], interval_secs: (1e-30, 1e-30), distance_m: (1.0, 2.0) )]
        )"#;
        assert_eq!(
            AmbienceDef::from_ron(text).map(|_| ()).map_err(|e| e.code()),
            Err("ambience_invalid")
        );
        // A sane small interval at the floor still validates.
        let ok = text.replace("1e-30, 1e-30", "0.01, 0.02");
        AmbienceDef::from_ron(&ok).expect("the minimum interval validates");
    }

    #[test]
    fn bad_files_are_refused_with_codes() {
        for (text, code) in [
            (
                "( version: 1, name: \"x\", one_shots: [( name: \"a\", sounds: [], interval_secs: (1.0, 2.0), distance_m: (1.0, 2.0) )] )",
                "ambience_invalid",
            ),
            (
                "( version: 1, name: \"x\", one_shots: [( name: \"a\", sounds: [\"s\"], interval_secs: (0.0, 2.0), distance_m: (1.0, 2.0) )] )",
                "ambience_invalid",
            ),
            (
                "( version: 1, name: \"x\", one_shots: [( name: \"a\", sounds: [\"s\"], interval_secs: (3.0, 2.0), distance_m: (1.0, 2.0) )] )",
                "ambience_invalid",
            ),
            (
                "( version: 1, name: \"x\", one_shots: [( name: \"a\", sounds: [\"s\"], interval_secs: (1.0, 2.0), distance_m: (-1.0, 2.0) )] )",
                "ambience_invalid",
            ),
            ("( version: 1, name: \"x\", volume_db: inf )", "ambience_invalid"),
            ("( version: 1, name: \"\" )", "ambience_invalid"),
            ("( version: 9, name: \"x\" )", "ambience_version"),
            ("( version: 1, name: \"x\", unknown: 3 )", "ambience_parse"),
            ("", "ambience_parse"),
            (
                "( version: 1, name: \"x\", beds: [(name: \"a\", drive: Wind, curve: [])], one_shots: [( name: \"a\", sounds: [\"s\"], interval_secs: (1.0, 2.0), distance_m: (1.0, 2.0) )] )",
                "ambience_invalid",
            ),
        ] {
            let got = AmbienceDef::from_ron(text).map(|_| ()).map_err(|e| e.code());
            assert_eq!(got, Err(code), "{text}");
        }
        assert!(ambience_migration_sources().is_empty());
        assert!(ambience_schema_fingerprint().contains("one_shots"));
    }
}
