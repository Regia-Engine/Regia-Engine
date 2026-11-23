// SPDX-License-Identifier: MPL-2.0
//! Contact sounds (ADR-3270): what physics objects sound like when they hit, slide and roll.
//!
//! An [`ImpactBank`] maps an unordered pair of physics-material names to an [`ImpactSet`]: impact
//! one-shot variants, a scrape loop, a roll loop and break one-shots. A pair resolves through
//! fallbacks: the pair's own entry, then the first material's set, then the second's, then the
//! bank's default material. The caller passes the material of the body that moves faster first,
//! so a wooden crate landing on concrete sounds like wood unless the pair has its own entry.
//!
//! The bank is pure apart from [`ImpactBank::play_impact`] and [`ImpactBank::play_break`]: the
//! level curves ([`ImpactCurves`]) turn an impulse and a relative normal speed into an impact
//! level and pitch, slip speed times normal force into a scrape level, and rolling speed (angular
//! speed times radius) into a roll level. The [`ImpactBudget`] holds the per-body cooldown, the
//! per-step cap on new impacts and the loop fade; `regia_framework::physics_audio` applies them to
//! physics contacts.
//!
//! The bank is a `.impactbank.ron` file (v1, `docs/formats/impactbank.md`): an [`ImpactBankDef`].
//!
//! Provenance: the curves are original design (levels in dB per doubling, like the footstep bank of
//! ADR-2442); the impact pitch exponent follows the Hertz contact time `t ∝ v^(-1/5)` (Hertz 1882,
//! as in Johnson, *Contact Mechanics*, 1985, § 11.4), and the scrape level follows the friction
//! power `μ N v` (textbook dry friction).

use std::collections::{BTreeMap, HashMap, VecDeque};

use regia_core::math::{Rng, Vec3};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::audio::Audio;
use crate::error::AudioError;
use crate::sound::{LoopRegion, PlaybackParams};
use crate::sounds::SoundAssets;
use crate::spatial::{AudioEmitter, RolloffCurve};

/// Current `.impactbank.ron` format version.
pub const IMPACT_BANK_FORMAT_VERSION: u32 = 1;

/// How many recent impacts and breaks [`ImpactBank::recent`] keeps.
pub const IMPACT_LOG_LEN: usize = 64;

/// How many `(body, set)` variant histories [`ImpactBank`] keeps. A long session touched
/// thousands of bodies otherwise: one entry per body and set, never dropped. At the cap a new
/// key evicts the oldest other key, so a body keeps its no-repeat while it keeps impacting.
pub const IMPACT_HISTORY_CAP: usize = 4096;

/// Why a `.impactbank.ron` did not load or a bank is invalid.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ImpactBankError {
    /// The text is not valid RON for the format.
    #[error("impact bank does not parse: {0}")]
    Parse(String),
    /// The file's `format_version` is not the current one.
    #[error("impact bank format version {found}; this build reads {expected}")]
    Version {
        /// The version in the file.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },
    /// A material or pair name is empty.
    #[error("impact bank has an empty material name ({0})")]
    EmptyName(&'static str),
    /// A number is out of range or not finite.
    #[error("impact bank field `{0}` is out of range or not finite")]
    Value(&'static str),
    /// A set lists more than [`ImpactSet::MAX_VARIANTS`] variants, or an empty sound name.
    #[error("impact set `{0}` has an empty sound name or more than 64 variants")]
    Sounds(String),
}

impl ImpactBankError {
    /// A stable identifier: lowercase ASCII, digits and `_` (agent protocol, logs).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "impactbank_parse",
            Self::Version { .. } => "impactbank_version",
            Self::EmptyName(_) => "impactbank_name",
            Self::Value(_) => "impactbank_value",
            Self::Sounds(_) => "impactbank_sounds",
        }
    }
}

/// The sounds of one material (or one material pair).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImpactSet {
    /// Impact one-shot variants, by [`SoundAssets`] name. Four or more avoid an audible pattern.
    pub impacts: Vec<String>,
    /// The scrape loop (sliding contact); `None` slides silently.
    pub scrape: Option<String>,
    /// The roll loop (rolling contact); `None` rolls silently.
    pub roll: Option<String>,
    /// Break one-shot variants (destruction); empty uses [`Self::impacts`].
    pub breaks: Vec<String>,
    /// Base level of every sound of the set, dB.
    pub volume_db: f32,
    /// Base pitch ratio.
    pub pitch: f32,
    /// Random pitch spread of one-shots, ± this fraction (0.05 = ±5 %).
    pub pitch_jitter: f32,
    /// Random level spread of one-shots, ± dB.
    pub volume_jitter_db: f32,
}

impl Default for ImpactSet {
    /// No sounds, 0 dB, pitch 1, ±5 % pitch and ±1.5 dB spread.
    fn default() -> Self {
        Self {
            impacts: Vec::new(),
            scrape: None,
            roll: None,
            breaks: Vec::new(),
            volume_db: 0.0,
            pitch: 1.0,
            pitch_jitter: 0.05,
            volume_jitter_db: 1.5,
        }
    }
}

impl ImpactSet {
    /// Most variants one list may hold.
    pub const MAX_VARIANTS: usize = 64;

    /// A set with these impact variants and the default spread.
    #[must_use]
    pub fn new(impacts: Vec<String>) -> Self {
        Self {
            impacts,
            ..Self::default()
        }
    }
}

/// A pair entry: the set two materials play together, whichever comes first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ImpactPair {
    /// One material.
    pub a: String,
    /// The other material.
    pub b: String,
    /// What they sound like together.
    pub set: ImpactSet,
}

/// The documented level and pitch curves (ADR-3270). Levels are dB relative to the set's level;
/// every curve is `db_per_doubling × log2(value / reference)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImpactCurves {
    /// Impulses below this (N·s) make no impact.
    pub min_impulse_ns: f32,
    /// Relative normal (approach) speeds below this (m/s) make no impact: a resting or jittering
    /// contact never retriggers.
    pub min_normal_speed_m_s: f32,
    /// The impulse that plays at the set's level (with the reference speed), N·s.
    pub reference_impulse_ns: f32,
    /// Level change per doubling of the impulse, dB.
    pub impulse_db_per_doubling: f32,
    /// The approach speed that plays at the set's level (with the reference impulse), m/s.
    pub reference_speed_m_s: f32,
    /// Level change per doubling of the approach speed, dB.
    pub speed_db_per_doubling: f32,
    /// The impulse and speed terms together stay within `−max_cut_db..=max_boost_db`.
    pub max_boost_db: f32,
    /// See [`Self::max_boost_db`].
    pub max_cut_db: f32,
    /// Impact pitch: `(speed / reference)^exponent` (a harder hit is brighter; Hertz contact time
    /// shrinks as `v^(-1/5)`), within 0.8..1.25.
    pub speed_pitch_exponent: f32,
    /// Slip speeds below this (m/s) do not scrape.
    pub scrape_min_speed_m_s: f32,
    /// The friction power (slip speed × normal force, W) that scrapes at the set's level.
    pub scrape_reference_power_w: f32,
    /// Scrape level change per doubling of the power, dB.
    pub scrape_db_per_doubling: f32,
    /// Scrape pitch: `(slip / scrape_pitch_reference_m_s)^scrape_pitch_exponent`, within 0.5..2.
    pub scrape_pitch_reference_m_s: f32,
    /// See [`Self::scrape_pitch_reference_m_s`].
    pub scrape_pitch_exponent: f32,
    /// Rolling speeds (angular speed × radius) below this (m/s) do not roll.
    pub roll_min_speed_m_s: f32,
    /// The rolling speed that rolls at the set's level, m/s.
    pub roll_reference_speed_m_s: f32,
    /// Roll level change per doubling of the rolling speed, dB.
    pub roll_db_per_doubling: f32,
    /// Roll pitch: `(speed / roll_reference_speed_m_s)^roll_pitch_exponent`, within 0.5..2.
    pub roll_pitch_exponent: f32,
    /// A contact rolls (rather than slides) while its slip speed is at most this fraction of its
    /// rolling speed.
    pub roll_slip_ratio: f32,
    /// Loops quieter than this (dB below the set's level) are silent.
    pub loop_floor_db: f32,
    /// Loops never play louder than this above the set's level, dB.
    pub loop_max_db: f32,
}

impl Default for ImpactCurves {
    fn default() -> Self {
        Self {
            min_impulse_ns: 0.05,
            min_normal_speed_m_s: 0.25,
            reference_impulse_ns: 10.0,
            impulse_db_per_doubling: 4.0,
            reference_speed_m_s: 5.0,
            speed_db_per_doubling: 2.0,
            max_boost_db: 12.0,
            max_cut_db: 40.0,
            speed_pitch_exponent: 0.1,
            scrape_min_speed_m_s: 0.05,
            scrape_reference_power_w: 20.0,
            scrape_db_per_doubling: 3.0,
            scrape_pitch_reference_m_s: 1.0,
            scrape_pitch_exponent: 0.3,
            roll_min_speed_m_s: 0.05,
            roll_reference_speed_m_s: 2.0,
            roll_db_per_doubling: 6.0,
            roll_pitch_exponent: 0.5,
            roll_slip_ratio: 0.25,
            loop_floor_db: -40.0,
            loop_max_db: 6.0,
        }
    }
}

/// How many contact sounds may start, and how loops end (ADR-3270).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImpactBudget {
    /// Fixed steps after an impact during which the same dynamic body starts no other impact.
    pub cooldown_steps: u32,
    /// New impacts per fixed step; the quietest beyond it are dropped and counted.
    pub max_new_impacts_per_step: u32,
    /// Fixed steps a scrape or roll loop takes to fade out after its contact stops sliding or
    /// rolling (or ends); it is silent and stopped at the end.
    pub fade_steps: u32,
    /// Scrape and roll loops at once; the quietest pairs beyond it stay silent and are counted.
    pub max_loop_voices: u32,
}

impl Default for ImpactBudget {
    fn default() -> Self {
        Self {
            cooldown_steps: 6,
            max_new_impacts_per_step: 8,
            fade_steps: 6,
            max_loop_voices: 16,
        }
    }
}

/// A `.impactbank.ron`: the sets, curves and budget of contact sounds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImpactBankDef {
    /// Must be [`IMPACT_BANK_FORMAT_VERSION`].
    pub format_version: u32,
    /// Material → set.
    pub materials: BTreeMap<String, ImpactSet>,
    /// Pair entries (checked before either material's own set).
    pub pairs: Vec<ImpactPair>,
    /// The material whose set plays when neither material of a contact has one.
    pub default_material: String,
    /// Destruction `fx_tag` → impact material (a tag not listed uses its own name).
    pub destruction_materials: BTreeMap<String, String>,
    /// The level and pitch curves.
    pub curves: ImpactCurves,
    /// The voice budget.
    pub budget: ImpactBudget,
    /// The bus contact sounds play on.
    pub bus: String,
    /// Distance beyond which contact sounds are not heard, metres.
    pub max_distance_m: f32,
}

impl Default for ImpactBankDef {
    fn default() -> Self {
        Self {
            format_version: IMPACT_BANK_FORMAT_VERSION,
            materials: BTreeMap::new(),
            pairs: Vec::new(),
            default_material: "default".into(),
            destruction_materials: BTreeMap::new(),
            curves: ImpactCurves::default(),
            budget: ImpactBudget::default(),
            bus: "World".into(),
            max_distance_m: 40.0,
        }
    }
}

impl ImpactBankDef {
    /// Parses and validates a `.impactbank.ron` text.
    ///
    /// # Errors
    /// [`ImpactBankError`]: a parse failure, another version, an empty name or a bad value.
    pub fn from_ron(text: &str) -> Result<Self, ImpactBankError> {
        let found = regia_core::version::peek_ron_version(text).map_err(ImpactBankError::Parse)?;
        if found != IMPACT_BANK_FORMAT_VERSION {
            return Err(ImpactBankError::Version {
                found,
                expected: IMPACT_BANK_FORMAT_VERSION,
            });
        }
        let def: Self = ron::from_str(text).map_err(|e| ImpactBankError::Parse(e.to_string()))?;
        def.validate()?;
        Ok(def)
    }

    /// The bank as pretty RON text.
    ///
    /// # Errors
    /// [`ImpactBankError::Parse`] when RON cannot write the value (never for a valid one).
    pub fn to_ron(&self) -> Result<String, ImpactBankError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| ImpactBankError::Parse(e.to_string()))
    }

    /// Checks names, sound lists and ranges.
    ///
    /// # Errors
    /// [`ImpactBankError::EmptyName`], [`ImpactBankError::Sounds`] or [`ImpactBankError::Value`].
    pub fn validate(&self) -> Result<(), ImpactBankError> {
        if self.default_material.trim().is_empty() {
            return Err(ImpactBankError::EmptyName("default_material"));
        }
        for (name, set) in &self.materials {
            if name.trim().is_empty() {
                return Err(ImpactBankError::EmptyName("materials"));
            }
            validate_set(name, set)?;
        }
        for pair in &self.pairs {
            if pair.a.trim().is_empty() || pair.b.trim().is_empty() {
                return Err(ImpactBankError::EmptyName("pairs"));
            }
            validate_set(&format!("{}|{}", pair.a, pair.b), &pair.set)?;
        }
        for (tag, material) in &self.destruction_materials {
            if tag.trim().is_empty() || material.trim().is_empty() {
                return Err(ImpactBankError::EmptyName("destruction_materials"));
            }
        }
        let c = &self.curves;
        let positive = |v: f32| v.is_finite() && v > 0.0;
        let non_negative = |v: f32| v.is_finite() && v >= 0.0;
        let in_range = |v: f32, lo: f32, hi: f32| v.is_finite() && (lo..=hi).contains(&v);
        for (ok, field) in [
            (non_negative(c.min_impulse_ns), "min_impulse_ns"),
            (non_negative(c.min_normal_speed_m_s), "min_normal_speed_m_s"),
            (positive(c.reference_impulse_ns), "reference_impulse_ns"),
            (
                in_range(c.impulse_db_per_doubling, 0.0, 24.0),
                "impulse_db_per_doubling",
            ),
            (positive(c.reference_speed_m_s), "reference_speed_m_s"),
            (
                in_range(c.speed_db_per_doubling, 0.0, 24.0),
                "speed_db_per_doubling",
            ),
            (in_range(c.max_boost_db, 0.0, 48.0), "max_boost_db"),
            (in_range(c.max_cut_db, 0.0, 96.0), "max_cut_db"),
            (in_range(c.speed_pitch_exponent, 0.0, 1.0), "speed_pitch_exponent"),
            (non_negative(c.scrape_min_speed_m_s), "scrape_min_speed_m_s"),
            (positive(c.scrape_reference_power_w), "scrape_reference_power_w"),
            (
                in_range(c.scrape_db_per_doubling, 0.0, 24.0),
                "scrape_db_per_doubling",
            ),
            (
                positive(c.scrape_pitch_reference_m_s),
                "scrape_pitch_reference_m_s",
            ),
            (
                in_range(c.scrape_pitch_exponent, 0.0, 1.0),
                "scrape_pitch_exponent",
            ),
            (non_negative(c.roll_min_speed_m_s), "roll_min_speed_m_s"),
            (positive(c.roll_reference_speed_m_s), "roll_reference_speed_m_s"),
            (
                in_range(c.roll_db_per_doubling, 0.0, 24.0),
                "roll_db_per_doubling",
            ),
            (in_range(c.roll_pitch_exponent, 0.0, 1.0), "roll_pitch_exponent"),
            (in_range(c.roll_slip_ratio, 0.0, 1.0), "roll_slip_ratio"),
            (in_range(c.loop_floor_db, -96.0, 0.0), "loop_floor_db"),
            (in_range(c.loop_max_db, 0.0, 24.0), "loop_max_db"),
            (positive(self.max_distance_m), "max_distance_m"),
        ] {
            if !ok {
                return Err(ImpactBankError::Value(field));
            }
        }
        let b = &self.budget;
        for (ok, field) in [
            (b.cooldown_steps <= 600, "cooldown_steps"),
            (
                (1..=256).contains(&b.max_new_impacts_per_step),
                "max_new_impacts_per_step",
            ),
            ((1..=600).contains(&b.fade_steps), "fade_steps"),
            (b.max_loop_voices <= 256, "max_loop_voices"),
        ] {
            if !ok {
                return Err(ImpactBankError::Value(field));
            }
        }
        if self.bus.trim().is_empty() {
            return Err(ImpactBankError::EmptyName("bus"));
        }
        Ok(())
    }
}

fn validate_set(name: &str, set: &ImpactSet) -> Result<(), ImpactBankError> {
    let bad_list =
        |list: &[String]| list.len() > ImpactSet::MAX_VARIANTS || list.iter().any(|s| s.trim().is_empty());
    let bad_one = |s: &Option<String>| s.as_ref().is_some_and(|s| s.trim().is_empty());
    if bad_list(&set.impacts) || bad_list(&set.breaks) || bad_one(&set.scrape) || bad_one(&set.roll) {
        return Err(ImpactBankError::Sounds(name.to_string()));
    }
    let in_range = |v: f32, lo: f32, hi: f32| v.is_finite() && (lo..=hi).contains(&v);
    for (ok, field) in [
        (in_range(set.volume_db, -60.0, 12.0), "volume_db"),
        (in_range(set.pitch, 0.25, 4.0), "pitch"),
        (in_range(set.pitch_jitter, 0.0, 0.5), "pitch_jitter"),
        (in_range(set.volume_jitter_db, 0.0, 12.0), "volume_jitter_db"),
    ] {
        if !ok {
            return Err(ImpactBankError::Value(field));
        }
    }
    Ok(())
}

/// Versions a migrator accepts below the current one (none at v1).
#[must_use]
pub fn impactbank_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Serde field-list fingerprint of `.impactbank.ron` (for `cargo xtask check-formats`).
#[must_use]
pub fn impactbank_schema_fingerprint() -> String {
    use regia_core::schema::serde_field_names;
    regia_core::schema::join_field_lists(&[
        serde_field_names::<ImpactBankDef>(),
        serde_field_names::<ImpactSet>(),
        serde_field_names::<ImpactPair>(),
        serde_field_names::<ImpactCurves>(),
        serde_field_names::<ImpactBudget>(),
    ])
}

/// Where a contact's set came from (the fallback chain of [`ImpactBank::resolve`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ImpactSource {
    /// The pair's own entry.
    Pair,
    /// The first material's set.
    First,
    /// The second material's set.
    Second,
    /// The bank's default material.
    Default,
}

impl ImpactSource {
    /// Lowercase name for logs and the agent protocol.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pair => "pair",
            Self::First => "first",
            Self::Second => "second",
            Self::Default => "default",
        }
    }
}

/// A resolved set: where it came from and the key that names it in histories and logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SetKey {
    /// `pairs[index]`.
    Pair(usize),
    /// A material set, by its index in name order.
    Material(usize),
}

/// What kind of loop a sliding or rolling contact plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ContactLoop {
    /// Sliding: the scrape loop.
    Scrape,
    /// Rolling: the roll loop.
    Roll,
}

impl ContactLoop {
    /// Lowercase name for logs and the agent protocol.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scrape => "scrape",
            Self::Roll => "roll",
        }
    }
}

/// One contact the bank turns into an impact (borrowed names, no allocation).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImpactContact<'a> {
    /// The body whose cooldown and variant history it uses (an entity's bits).
    pub body: u64,
    /// The faster body's material.
    pub material_a: &'a str,
    /// The other body's material.
    pub material_b: &'a str,
    /// The contact's impulse this step, N·s (the framework uses the larger of force × step and the
    /// momentum change of the approach).
    pub impulse_ns: f32,
    /// Relative normal (approach) speed before the step, m/s.
    pub normal_speed_m_s: f32,
    /// Where the bodies touch, world metres.
    pub position: Vec3,
}

/// What an impact or break played (or would play, [`ImpactBank::plan_impact`]).
#[derive(Clone, Debug, PartialEq)]
pub struct ImpactPlay {
    /// The body (entity bits) it was charged to; 0 for a break.
    pub body: u64,
    /// `impact` or `break`.
    pub kind: &'static str,
    /// The two materials as given (the break's tag and its impact material for a break).
    pub materials: (String, String),
    /// Where the set came from.
    pub source: ImpactSource,
    /// The variant index in the list.
    pub variant: usize,
    /// The variant's sound name.
    pub sound: String,
    /// The curve level before the spread, dB relative to the set (what the curves predict).
    pub curve_db: f32,
    /// The level played, dB (set level, curve and spread).
    pub volume_db: f32,
    /// The pitch played (set, curve and spread).
    pub pitch: f32,
    /// The impulse, N·s (the break's magnitude for a break).
    pub impulse_ns: f32,
    /// The approach speed, m/s (0 for a break).
    pub normal_speed_m_s: f32,
    /// Where it played.
    pub position: Vec3,
    /// A voice started (false for a plan, a missing sound or out of range).
    pub started: bool,
}

/// A loop's level and pitch for one step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopLevel {
    /// Scrape or roll.
    pub kind: ContactLoop,
    /// Level relative to the set, dB (above [`ImpactCurves::loop_floor_db`]).
    pub db: f32,
    /// Pitch ratio relative to the set.
    pub pitch: f32,
}

/// The contact-sound bank (opt-in resource): the [`ImpactBankDef`] plus the variant generator, the
/// per-body variant history and the play log.
#[derive(Resource, Clone, Debug)]
pub struct ImpactBank {
    def: ImpactBankDef,
    /// Material names in order (index = [`SetKey::Material`]).
    material_index: Vec<String>,
    /// The emitter contact sounds play through (range, rolloff, bus).
    pub emitter: AudioEmitter,
    /// The last variant per `(body, set, breaking)` key (breaks never store one), each with its
    /// insertion sequence. The map never grows past [`IMPACT_HISTORY_CAP`]: a new key evicts the
    /// oldest other key.
    history: HashMap<(u64, SetKey, bool), (usize, u64)>,
    /// The next insertion sequence for `history` (monotonic; makes the oldest key unique, so
    /// eviction never depends on hash order).
    history_seq: u64,
    rng: Rng,
    recent: VecDeque<ImpactPlay>,
    played: u64,
    breaks: u64,
}

impl Default for ImpactBank {
    fn default() -> Self {
        Self::new(ImpactBankDef::default())
    }
}

/// The seed a new bank's variant generator starts from.
const IMPACT_SEED: u64 = 0x1A9C_7B0A_4E5D_0001;

/// Level and pitch scalars copied off an [`ImpactSet`] so a plan can drop the set before it
/// draws. The sound name is cloned once, after the draw.
struct SetLevels {
    volume_db: f32,
    pitch: f32,
    pitch_jitter: f32,
    volume_jitter_db: f32,
}

impl SetLevels {
    fn from_set(set: &ImpactSet) -> Self {
        Self {
            volume_db: set.volume_db,
            pitch: set.pitch,
            pitch_jitter: set.pitch_jitter,
            volume_jitter_db: set.volume_jitter_db,
        }
    }
}

impl ImpactBank {
    /// A bank over `def` (not validated: use [`ImpactBankDef::from_ron`] or
    /// [`ImpactBankDef::validate`] for authored data).
    #[must_use]
    pub fn new(def: ImpactBankDef) -> Self {
        let material_index = def.materials.keys().cloned().collect();
        let emitter = AudioEmitter {
            min_distance: 1.0,
            max_distance: def.max_distance_m,
            rolloff: RolloffCurve::Log { db_per_doubling: 6.0 },
            air_absorption: true,
            bus: Some(def.bus.clone()),
            autoplay: false,
            ..AudioEmitter::default()
        };
        Self {
            def,
            material_index,
            emitter,
            history: HashMap::new(),
            history_seq: 0,
            rng: Rng::new(IMPACT_SEED),
            recent: VecDeque::with_capacity(IMPACT_LOG_LEN),
            played: 0,
            breaks: 0,
        }
    }

    /// Parses a `.impactbank.ron` text into a bank.
    ///
    /// # Errors
    /// [`ImpactBankError`] from [`ImpactBankDef::from_ron`].
    pub fn from_ron(text: &str) -> Result<Self, ImpactBankError> {
        ImpactBankDef::from_ron(text).map(Self::new)
    }

    /// The definition (sets, curves, budget).
    #[must_use]
    pub fn def(&self) -> &ImpactBankDef {
        &self.def
    }

    /// The curves.
    #[must_use]
    pub fn curves(&self) -> &ImpactCurves {
        &self.def.curves
    }

    /// The budget.
    #[must_use]
    pub fn budget(&self) -> &ImpactBudget {
        &self.def.budget
    }

    /// Replaces the definition, keeping the generator, the log and the counters; the variant
    /// history is forgotten (set indices may have moved).
    pub fn set_def(&mut self, def: ImpactBankDef) {
        let rng = self.rng.clone();
        let recent = std::mem::take(&mut self.recent);
        let (played, breaks) = (self.played, self.breaks);
        *self = Self::new(def);
        self.rng = rng;
        self.recent = recent;
        self.played = played;
        self.breaks = breaks;
    }

    /// Reseeds the variant and spread generator and forgets every body's history.
    pub fn reseed(&mut self, seed: u64) {
        self.rng = Rng::new(seed);
        self.history.clear();
    }

    /// The set a contact between materials `a` (the faster body's) and `b` plays: the pair's
    /// entry (either order), then `a`'s set, then `b`'s, then the default material's. `None`
    /// when none exists.
    #[must_use]
    pub fn resolve(&self, a: &str, b: &str) -> Option<(SetKey, ImpactSource, &ImpactSet)> {
        if let Some((i, pair)) = self
            .def
            .pairs
            .iter()
            .enumerate()
            .find(|(_, p)| (p.a == a && p.b == b) || (p.a == b && p.b == a))
        {
            return Some((SetKey::Pair(i), ImpactSource::Pair, &pair.set));
        }
        for (name, source) in [
            (a, ImpactSource::First),
            (b, ImpactSource::Second),
            (self.def.default_material.as_str(), ImpactSource::Default),
        ] {
            if let Ok(i) = self.material_index.binary_search_by(|m| m.as_str().cmp(name))
                && let Some(set) = self.def.materials.get(name)
            {
                return Some((SetKey::Material(i), source, set));
            }
        }
        None
    }

    /// The impact level of an impulse and approach speed, dB relative to the set, within the
    /// curve limits; `None` below the minimum impulse or approach speed (no impact).
    #[must_use]
    pub fn impact_db(&self, impulse_ns: f32, normal_speed_m_s: f32) -> Option<f32> {
        let c = &self.def.curves;
        if !(impulse_ns.is_finite() && normal_speed_m_s.is_finite())
            || impulse_ns < c.min_impulse_ns
            || normal_speed_m_s < c.min_normal_speed_m_s
            || impulse_ns <= 0.0
            || normal_speed_m_s <= 0.0
        {
            return None;
        }
        let db = c.impulse_db_per_doubling * log2_ratio(impulse_ns, c.reference_impulse_ns)
            + c.speed_db_per_doubling * log2_ratio(normal_speed_m_s, c.reference_speed_m_s);
        let db = if db.is_finite() { db } else { 0.0 };
        Some(clamp_curve_db(db, c))
    }

    /// The impact pitch factor of an approach speed: `(speed / reference)^exponent`, within 0.8..1.25.
    #[must_use]
    pub fn impact_pitch(&self, normal_speed_m_s: f32) -> f32 {
        let c = &self.def.curves;
        let p = (log2_ratio(normal_speed_m_s, c.reference_speed_m_s) * c.speed_pitch_exponent).exp2();
        if p.is_finite() { p.clamp(0.8, 1.25) } else { 1.0 }
    }

    /// The scrape level of a sliding contact: `None` below the minimum slip or the loop floor.
    #[must_use]
    pub fn scrape_level(&self, slip_m_s: f32, normal_force_n: f32) -> Option<LoopLevel> {
        let c = &self.def.curves;
        if !(slip_m_s.is_finite() && normal_force_n.is_finite()) || slip_m_s < c.scrape_min_speed_m_s {
            return None;
        }
        let power = slip_m_s * normal_force_n.max(0.0);
        if power <= 0.0 {
            return None;
        }
        let db = c.scrape_db_per_doubling * log2_ratio(power, c.scrape_reference_power_w);
        let pitch = (log2_ratio(slip_m_s, c.scrape_pitch_reference_m_s) * c.scrape_pitch_exponent).exp2();
        self.loop_level(ContactLoop::Scrape, db, pitch)
    }

    /// The roll level of a rolling contact (rolling speed = angular speed × radius): `None` below
    /// the minimum speed or the loop floor.
    #[must_use]
    pub fn roll_level(&self, rolling_speed_m_s: f32) -> Option<LoopLevel> {
        let c = &self.def.curves;
        if !rolling_speed_m_s.is_finite() || rolling_speed_m_s < c.roll_min_speed_m_s {
            return None;
        }
        let ratio = log2_ratio(rolling_speed_m_s, c.roll_reference_speed_m_s);
        let db = c.roll_db_per_doubling * ratio;
        let pitch = (ratio * c.roll_pitch_exponent).exp2();
        self.loop_level(ContactLoop::Roll, db, pitch)
    }

    fn loop_level(&self, kind: ContactLoop, db: f32, pitch: f32) -> Option<LoopLevel> {
        let c = &self.def.curves;
        if !db.is_finite() || db < c.loop_floor_db {
            return None;
        }
        Some(LoopLevel {
            kind,
            db: db.min(c.loop_max_db),
            pitch: if pitch.is_finite() {
                pitch.clamp(0.5, 2.0)
            } else {
                1.0
            },
        })
    }

    /// Whether a moving contact rolls or slides, and how loud: it rolls while its rolling speed
    /// (angular speed × radius) is at least the minimum and its slip is at most
    /// `roll_slip_ratio` of the rolling speed; otherwise it scrapes with its slip and normal force.
    #[must_use]
    pub fn contact_loop(
        &self,
        slip_m_s: f32,
        rolling_speed_m_s: f32,
        normal_force_n: f32,
    ) -> Option<LoopLevel> {
        let c = &self.def.curves;
        if rolling_speed_m_s.is_finite()
            && rolling_speed_m_s >= c.roll_min_speed_m_s
            && slip_m_s.is_finite()
            && slip_m_s <= c.roll_slip_ratio * rolling_speed_m_s
        {
            return self.roll_level(rolling_speed_m_s);
        }
        self.scrape_level(slip_m_s, normal_force_n)
    }

    /// The loop sound of `kind` for a contact between `a` and `b` (the set resolved like an
    /// impact): `(sound, set level dB, set pitch)`.
    #[must_use]
    pub fn loop_sound(&self, a: &str, b: &str, kind: ContactLoop) -> Option<(&str, f32, f32)> {
        let (_, _, set) = self.resolve(a, b)?;
        let sound = match kind {
            ContactLoop::Scrape => set.scrape.as_deref(),
            ContactLoop::Roll => set.roll.as_deref(),
        }?;
        Some((sound, set.volume_db, set.pitch.max(0.01)))
    }

    /// Picks the variant, level and pitch of an impact without playing it; advances the body's
    /// history (its next impact on that set will not repeat the variant). `None` below the
    /// curve minimums or when no set (or no variant) resolves.
    pub fn plan_impact(&mut self, contact: &ImpactContact<'_>) -> Option<ImpactPlay> {
        let curve_db = self.impact_db(contact.impulse_ns, contact.normal_speed_m_s)?;
        let pitch_curve = self.impact_pitch(contact.normal_speed_m_s);
        let (key, source, list_len, levels) = {
            let (key, source, set) = self.resolve(contact.material_a, contact.material_b)?;
            (key, source, set.impacts.len(), SetLevels::from_set(set))
        };
        let (variant, volume_db, pitch) = self.pick_levels(
            (contact.body, key, false),
            list_len,
            &levels,
            (curve_db, pitch_curve),
        )?;
        let sound = self.variant_sound(contact.material_a, contact.material_b, variant, false)?;
        Some(ImpactPlay {
            body: contact.body,
            kind: "impact",
            materials: (contact.material_a.to_string(), contact.material_b.to_string()),
            source,
            variant,
            sound,
            curve_db,
            volume_db,
            pitch,
            impulse_ns: contact.impulse_ns,
            normal_speed_m_s: contact.normal_speed_m_s,
            position: contact.position,
            started: false,
        })
    }

    /// The impact material of a destruction `fx_tag`: its `destruction_materials` entry, else the
    /// tag itself.
    #[must_use]
    pub fn destruction_material<'a>(&'a self, fx_tag: &'a str) -> &'a str {
        self.def
            .destruction_materials
            .get(fx_tag)
            .map_or(fx_tag, String::as_str)
    }

    /// Plans a break of a destructible whose chunks carry `fx_tag`, `magnitude` (the bond's break
    /// impulse, N·s) at `position`; the set resolves like a contact of the mapped material with
    /// itself, and plays its `breaks` (else its `impacts`). The level uses the impact curve with
    /// the reference speed; `seed` (the destruction event's) picks the variant and spread.
    /// `None` when `magnitude` is not finite or no set resolves.
    pub fn plan_break(
        &mut self,
        fx_tag: &str,
        magnitude: f32,
        position: Vec3,
        seed: u64,
    ) -> Option<ImpactPlay> {
        // A non-finite magnitude is not an impulse. `f32::max` turns NaN into the floor and
        // `impact_db` turns infinity into "no curve", and the old `unwrap_or(0.0)` still played.
        if !magnitude.is_finite() {
            return None;
        }
        // A break has no approach speed: the speed term at the reference speed is zero, and the
        // impulse minimums do not gate it (they exist so a too-small contact is no impact at all;
        // a break happened). A magnitude at or below the floor is the quietest break, not a 0 dB
        // one, so it lands on the cut floor.
        let c = &self.def.curves;
        let impulse = magnitude.max(c.min_impulse_ns);
        let raw = if impulse > 0.0 {
            c.impulse_db_per_doubling * log2_ratio(impulse, c.reference_impulse_ns)
        } else {
            f32::NEG_INFINITY
        };
        let curve_db = if raw.is_nan() { 0.0 } else { clamp_curve_db(raw, c) };
        let (material, key, source, list_len, levels) = {
            let material = self.destruction_material(fx_tag).to_string();
            let (key, source, set) = self.resolve(&material, &material)?;
            let list_len = if set.breaks.is_empty() {
                set.impacts.len()
            } else {
                set.breaks.len()
            };
            (material, key, source, list_len, SetLevels::from_set(set))
        };
        // The event's own seed picks the variant and spread, so every peer that replays the same
        // break hears the same sound; the bank's generator does not move.
        let bank_rng = std::mem::replace(&mut self.rng, Rng::new(seed ^ IMPACT_SEED));
        let picked = self.pick_levels((0, key, true), list_len, &levels, (curve_db, 1.0));
        self.rng = bank_rng;
        let (variant, volume_db, pitch) = picked?;
        let sound = self.variant_sound(&material, &material, variant, true)?;
        Some(ImpactPlay {
            body: 0,
            kind: "break",
            materials: (fx_tag.to_string(), material),
            source,
            variant,
            sound,
            curve_db,
            volume_db,
            pitch,
            impulse_ns: magnitude,
            normal_speed_m_s: 0.0,
            position,
            started: false,
        })
    }

    /// Draws the variant and the spread around `(curve_db, pitch_curve)`. Same generator order as
    /// before: variant, then level jitter, then pitch jitter, then the body's history. Copies no
    /// sound name.
    fn pick_levels(
        &mut self,
        history_key: (u64, SetKey, bool),
        list_len: usize,
        set: &SetLevels,
        (curve_db, pitch_curve): (f32, f32),
    ) -> Option<(usize, f32, f32)> {
        // A break's pick depends on its own seed only (peers that replay it agree), so breaks keep
        // no history.
        let breaking = history_key.2;
        let last = if breaking {
            None
        } else {
            self.history.get(&history_key).map(|(variant, _)| *variant)
        };
        let variant = crate::ambience::pick_variant(&mut self.rng, list_len, last)?;
        let spread = |rng: &mut Rng, amount: f32| {
            let a = if amount.is_finite() { amount.abs() } else { 0.0 };
            (rng.f32() * 2.0 - 1.0) * a
        };
        let volume_db = set.volume_db + curve_db + spread(&mut self.rng, set.volume_jitter_db);
        let pitch =
            set.pitch.max(0.01) * pitch_curve * (1.0 + spread(&mut self.rng, set.pitch_jitter.min(0.5)));
        if !breaking {
            self.remember_variant(history_key, variant);
        }
        let volume_db = if volume_db.is_finite() {
            volume_db
        } else {
            crate::bus::MUTE_DB
        };
        let pitch = if pitch.is_finite() {
            pitch.clamp(0.25, 4.0)
        } else {
            1.0
        };
        Some((variant, volume_db, pitch))
    }

    /// Records the variant `key` last played. At [`IMPACT_HISTORY_CAP`] a new key evicts the
    /// oldest other key (by insertion sequence, unique, so the choice never depends on hash
    /// order and peers replaying the same impacts evict the same key); a key already in the map
    /// just refreshes, and the key being written is never the eviction candidate, so the body
    /// that just played keeps its no-repeat.
    fn remember_variant(&mut self, key: (u64, SetKey, bool), variant: usize) {
        if self.history.len() >= IMPACT_HISTORY_CAP && !self.history.contains_key(&key) {
            let mut oldest: Option<(u64, SetKey, bool)> = None;
            let mut oldest_seq = u64::MAX;
            for (other, (_, seq)) in &self.history {
                if *other != key && *seq < oldest_seq {
                    oldest_seq = *seq;
                    oldest = Some(*other);
                }
            }
            if let Some(other) = oldest {
                self.history.remove(&other);
            }
        }
        self.history.insert(key, (variant, self.history_seq));
        self.history_seq = self.history_seq.wrapping_add(1);
    }

    /// The chosen variant's name. `breaks` uses the break list when it is not empty.
    fn variant_sound(&self, a: &str, b: &str, variant: usize, breaks: bool) -> Option<String> {
        let (_, _, set) = self.resolve(a, b)?;
        let list = if breaks && !set.breaks.is_empty() {
            &set.breaks
        } else {
            &set.impacts
        };
        list.get(variant).cloned()
    }

    /// Plays a planned impact or break at its position through the emitter pipeline and logs it.
    /// A sound that is not registered is logged with `started: false` (nothing plays), as is a
    /// play out of the emitter's range; a play that started no voice is not counted as played.
    ///
    /// # Errors
    /// The mixer's error when the voice cannot start (an unknown bus).
    pub fn play(
        &mut self,
        audio: &mut Audio,
        sounds: Option<&SoundAssets>,
        mut play: ImpactPlay,
    ) -> Result<ImpactPlay, AudioError> {
        let _span = tracing::trace_span!("impact_play").entered();
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
        if play.started {
            if play.kind == "break" {
                self.breaks += 1;
            } else {
                self.played += 1;
            }
        }
        if self.recent.len() == IMPACT_LOG_LEN {
            self.recent.pop_front();
        }
        self.recent.push_back(play.clone());
        Ok(play)
    }

    /// Plans and plays an impact. `Ok(None)` when nothing resolves or it is below the minimums.
    ///
    /// # Errors
    /// The mixer's error when the voice cannot start (an unknown bus).
    pub fn play_impact(
        &mut self,
        audio: &mut Audio,
        sounds: Option<&SoundAssets>,
        contact: &ImpactContact<'_>,
    ) -> Result<Option<ImpactPlay>, AudioError> {
        match self.plan_impact(contact) {
            Some(play) => self.play(audio, sounds, play).map(Some),
            None => Ok(None),
        }
    }

    /// Plans and plays a break. `Ok(None)` when nothing resolves.
    ///
    /// # Errors
    /// The mixer's error when the voice cannot start (an unknown bus).
    pub fn play_break(
        &mut self,
        audio: &mut Audio,
        sounds: Option<&SoundAssets>,
        fx_tag: &str,
        magnitude: f32,
        position: Vec3,
        seed: u64,
    ) -> Result<Option<ImpactPlay>, AudioError> {
        match self.plan_break(fx_tag, magnitude, position, seed) {
            Some(play) => self.play(audio, sounds, play).map(Some),
            None => Ok(None),
        }
    }

    /// Starts a scrape or roll loop at `position` at `db` (absolute, set level included) and
    /// `pitch`; `Ok(None)` when the sound is not registered or out of range.
    ///
    /// # Errors
    /// The mixer's error when the voice cannot start (an unknown bus).
    pub fn start_loop(
        &self,
        audio: &mut Audio,
        sounds: Option<&SoundAssets>,
        sound: &str,
        db: f32,
        pitch: f32,
        position: Vec3,
    ) -> Result<Option<crate::SoundInstanceId>, AudioError> {
        let Some(asset) = sounds.and_then(|s| s.by_name(sound)) else {
            return Ok(None);
        };
        let params = PlaybackParams {
            bus: self.emitter.bus.clone(),
            volume_db: db,
            pitch,
            loop_region: Some(LoopRegion {
                start_secs: 0.0,
                end_secs: None,
            }),
            ..PlaybackParams::default()
        };
        audio.start_asset_at(asset, params, position, &self.emitter)
    }

    /// The latest impacts and breaks, oldest first (at most [`IMPACT_LOG_LEN`]).
    pub fn recent(&self) -> impl Iterator<Item = &ImpactPlay> {
        self.recent.iter()
    }

    /// Impacts played since the bank was made.
    #[must_use]
    pub fn played(&self) -> u64 {
        self.played
    }

    /// Breaks played since the bank was made.
    #[must_use]
    pub fn breaks_played(&self) -> u64 {
        self.breaks
    }
}

/// `log2(value / reference)`, 0 for a non-positive or non-finite input.
/// Clamps a curve level to the cut/boost limits. `f32::clamp` panics on a NaN bound, and an
/// unvalidated bank (`ImpactBank::new` does not validate) can carry one: a non-finite limit
/// disables that side of the clamp instead.
fn clamp_curve_db(db: f32, c: &ImpactCurves) -> f32 {
    let cut = if c.max_cut_db.is_finite() {
        c.max_cut_db.abs()
    } else {
        f32::INFINITY
    };
    let boost = if c.max_boost_db.is_finite() {
        c.max_boost_db.abs()
    } else {
        f32::INFINITY
    };
    db.clamp(-cut, boost)
}

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

    fn names(prefix: &str) -> Vec<String> {
        (0..4).map(|i| format!("{prefix}_{i}")).collect()
    }

    fn def() -> ImpactBankDef {
        let mut def = ImpactBankDef::default();
        def.materials.insert(
            "wood".into(),
            ImpactSet {
                scrape: Some("wood_scrape".into()),
                roll: Some("wood_roll".into()),
                ..ImpactSet::new(names("wood"))
            },
        );
        def.materials
            .insert("metal".into(), ImpactSet::new(names("metal")));
        def.materials
            .insert("default".into(), ImpactSet::new(names("thud")));
        def.pairs.push(ImpactPair {
            a: "metal".into(),
            b: "wood".into(),
            set: ImpactSet::new(names("clank")),
        });
        def.destruction_materials
            .insert("concrete_chunks".into(), "metal".into());
        def
    }

    fn contact<'a>(a: &'a str, b: &'a str, impulse: f32, speed: f32) -> ImpactContact<'a> {
        ImpactContact {
            body: 7,
            material_a: a,
            material_b: b,
            impulse_ns: impulse,
            normal_speed_m_s: speed,
            position: Vec3::ZERO,
        }
    }

    #[test]
    fn pairs_resolve_through_their_fallbacks() {
        let bank = ImpactBank::new(def());
        let source = |a: &str, b: &str| bank.resolve(a, b).map(|(_, s, _)| s);
        assert_eq!(source("metal", "wood"), Some(ImpactSource::Pair));
        assert_eq!(source("wood", "metal"), Some(ImpactSource::Pair), "unordered");
        assert_eq!(source("wood", "stone"), Some(ImpactSource::First));
        assert_eq!(source("stone", "wood"), Some(ImpactSource::Second));
        assert_eq!(source("stone", "ice"), Some(ImpactSource::Default));
        let mut empty = def();
        empty.materials.remove("default");
        assert_eq!(ImpactBank::new(empty).resolve("stone", "ice"), None);
    }

    #[test]
    fn harder_hits_are_louder_and_brighter() {
        let bank = ImpactBank::new(def());
        let soft = bank.impact_db(1.0, 1.0).expect("audible");
        let medium = bank.impact_db(4.0, 4.0).expect("audible");
        let hard = bank.impact_db(16.0, 12.0).expect("audible");
        assert!(soft < medium && medium < hard, "{soft} {medium} {hard}");
        assert_eq!(
            bank.impact_db(10.0, 5.0),
            Some(0.0),
            "the reference plays at the set level"
        );
        assert!(bank.impact_pitch(12.0) > bank.impact_pitch(1.0));
        assert!(bank.impact_db(0.01, 5.0).is_none(), "below the minimum impulse");
        assert!(
            bank.impact_db(5.0, 0.1).is_none(),
            "a resting contact does not retrigger"
        );
        assert!(bank.impact_db(f32::NAN, 5.0).is_none());
        assert!(bank.impact_db(1.0e9, 1.0e9).expect("audible") <= 12.0, "capped");
    }

    #[test]
    fn rolling_and_sliding_are_told_apart() {
        let bank = ImpactBank::new(def());
        // A sphere rolling at 2 m/s: no slip.
        let roll = bank.contact_loop(0.01, 2.0, 10.0).expect("rolls");
        assert_eq!(roll.kind, ContactLoop::Roll);
        assert!(roll.db.abs() < 1e-4, "{}", roll.db);
        // A box sliding at 2 m/s on 10 N: 20 W, the reference.
        let slide = bank.contact_loop(2.0, 0.0, 10.0).expect("slides");
        assert_eq!(slide.kind, ContactLoop::Scrape);
        assert!(slide.db.abs() < 1e-4, "{}", slide.db);
        // A skidding ball slips too much to roll.
        assert_eq!(
            bank.contact_loop(1.0, 2.0, 10.0).map(|l| l.kind),
            Some(ContactLoop::Scrape)
        );
        assert!(bank.contact_loop(0.0, 0.0, 10.0).is_none(), "at rest");
        assert!(bank.contact_loop(0.06, 0.0, 1.0e-6).is_none(), "below the floor");
        let faster = bank.scrape_level(4.0, 10.0).expect("scrapes");
        assert!(faster.db > slide.db && faster.pitch > slide.pitch);
        assert_eq!(
            bank.loop_sound("wood", "stone", ContactLoop::Roll).map(|s| s.0),
            Some("wood_roll")
        );
        assert_eq!(
            bank.loop_sound("stone", "ice", ContactLoop::Roll),
            None,
            "the default has no loop"
        );
    }

    #[test]
    fn a_body_never_repeats_a_variant_back_to_back_and_seeds_replay() {
        let mut bank = ImpactBank::new(def());
        let mut last = None;
        for _ in 0..500 {
            let play = bank
                .plan_impact(&contact("wood", "stone", 5.0, 5.0))
                .expect("plays");
            assert_ne!(Some(play.variant), last);
            last = Some(play.variant);
        }
        let mut a = ImpactBank::new(def());
        let mut b = ImpactBank::new(def());
        a.reseed(42);
        b.reseed(42);
        for _ in 0..50 {
            assert_eq!(
                a.plan_impact(&contact("wood", "stone", 5.0, 5.0)),
                b.plan_impact(&contact("wood", "stone", 5.0, 5.0))
            );
        }
    }

    #[test]
    fn history_is_capped_and_the_oldest_other_key_is_dropped() {
        let mut bank = ImpactBank::new(def());
        let (set_key, _, _) = bank.resolve("stone", "ice").expect("the default set");
        let key = |body: u64| (body, set_key, false);
        // CAP + 128 distinct bodies: the map stays at the cap, the oldest entries are gone and
        // the newest is present (its no-repeat survives its own insert).
        let n = IMPACT_HISTORY_CAP as u64 + 128;
        for body in 0..n {
            let mut c = contact("stone", "ice", 5.0, 5.0);
            c.body = body;
            bank.plan_impact(&c).expect("plays");
        }
        assert!(bank.history.len() <= IMPACT_HISTORY_CAP);
        assert_eq!(bank.history.len(), IMPACT_HISTORY_CAP);
        assert!(bank.history.contains_key(&key(n - 1)), "the newest key stays");
        assert!(
            !bank.history.contains_key(&key(0)) && !bank.history.contains_key(&key(127)),
            "the oldest keys were evicted in order"
        );
        assert!(bank.history.contains_key(&key(128)), "eviction stops at the cap");
        // A body still in the map keeps its no-repeat after other inserts passed the cap.
        let mut c = contact("stone", "ice", 5.0, 5.0);
        c.body = n - 1;
        let last = bank.plan_impact(&c).expect("plays").variant;
        let mut c = contact("stone", "ice", 5.0, 5.0);
        c.body = n - 1;
        assert_ne!(
            bank.plan_impact(&c).expect("plays").variant,
            last,
            "the surviving entry still suppresses a repeat"
        );
        // Replays of the same session evict identically: two banks fed the same contacts give
        // the same variants, and a refresh (existing key) evicts nobody.
        let mut a = ImpactBank::new(def());
        let mut b = ImpactBank::new(def());
        for body in 0..n {
            let mut c = contact("stone", "ice", 5.0, 5.0);
            c.body = body;
            assert_eq!(
                a.plan_impact(&c).expect("plays").variant,
                b.plan_impact(&c).expect("plays").variant
            );
        }
        assert_eq!(a.history.len(), b.history.len());
    }

    #[test]
    fn breaks_map_destruction_tags() {
        let mut bank = ImpactBank::new(def());
        let play = bank
            .plan_break("concrete_chunks", 50.0, Vec3::ONE, 3)
            .expect("a break");
        assert_eq!(play.kind, "break");
        assert_eq!(play.materials.1, "metal");
        assert!(play.sound.starts_with("metal_"));
        let again = bank
            .plan_break("concrete_chunks", 50.0, Vec3::ONE, 3)
            .expect("a break");
        assert_eq!(again.variant, play.variant, "the event seed alone picks a break");
        let own = bank.plan_break("wood", 50.0, Vec3::ONE, 3).expect("a break");
        assert!(
            own.sound.starts_with("wood_"),
            "an unlisted tag is its own material"
        );
        assert!(
            bank.plan_break("wood", f32::NAN, Vec3::ONE, 3).is_none(),
            "a non-finite break magnitude plays nothing"
        );
        assert!(bank.plan_break("wood", f32::INFINITY, Vec3::ONE, 3).is_none());
        assert!(bank.plan_break("wood", f32::NEG_INFINITY, Vec3::ONE, 3).is_none());
        assert_eq!(bank.breaks_played(), 0, "planning does not count a play");
    }

    #[test]
    fn the_format_round_trips_and_rejects_with_codes() {
        let def = def();
        let text = def.to_ron().expect("ron");
        assert_eq!(ImpactBankDef::from_ron(&text), Ok(def));
        let sparse = ImpactBankDef::from_ron("(format_version: 1)").expect("defaults");
        assert_eq!(sparse.budget, ImpactBudget::default());
        let code = |t: &str| ImpactBankDef::from_ron(t).expect_err("rejected").code();
        assert_eq!(code("(("), "impactbank_parse");
        assert_eq!(code("(format_version: 2)"), "impactbank_version");
        assert_eq!(
            code(r#"(format_version: 1, default_material: " ")"#),
            "impactbank_name"
        );
        assert_eq!(
            code("(format_version: 1, budget: (max_new_impacts_per_step: 0))"),
            "impactbank_value"
        );
        assert_eq!(
            code("(format_version: 1, curves: (reference_impulse_ns: -1.0))"),
            "impactbank_value"
        );
        assert_eq!(
            code(r#"(format_version: 1, materials: { "wood": (impacts: [""]) })"#),
            "impactbank_sounds"
        );
        let f = impactbank_schema_fingerprint();
        for field in [
            "format_version",
            "materials",
            "pairs",
            "curves",
            "budget",
            "min_impulse_ns",
            "fade_steps",
        ] {
            assert!(f.contains(field), "{field} missing from {f}");
        }
    }

    /// `ImpactBank::new` takes a definition as is (it does not validate). A NaN level bound must
    /// not panic the curve: `f32::clamp` asserts on a NaN bound, so `impact_db`, `plan_impact` and
    /// `plan_break` used to panic. The level stays finite instead.
    #[test]
    fn a_nan_level_bound_in_an_unvalidated_bank_does_not_panic() {
        for (cut, boost) in [(f32::NAN, 12.0), (40.0, f32::NAN), (f32::NAN, f32::NAN)] {
            let mut def = def();
            def.curves.max_cut_db = cut;
            def.curves.max_boost_db = boost;
            assert!(def.validate().is_err(), "the authored path refuses it");
            let mut bank = ImpactBank::new(def);
            for (impulse, speed) in [(10.0f32, 5.0f32), (0.1, 0.5), (1.0e6, 50.0)] {
                let db = bank.impact_db(impulse, speed).expect("above the minimums");
                assert!(db.is_finite(), "cut {cut}, boost {boost}: {db}");
            }
            let play = bank
                .plan_impact(&contact("wood", "stone", 5.0, 5.0))
                .expect("plays");
            assert!(play.volume_db.is_finite(), "{}", play.volume_db);
            let broke = bank.plan_break("wood", 50.0, Vec3::ZERO, 1).expect("a break");
            assert!(broke.curve_db.is_finite(), "{}", broke.curve_db);
        }
    }

    /// A break's level follows its magnitude on the impact curve at the reference speed. Two
    /// valid banks broke that: with `min_impulse_ns: 0` a break of magnitude 0 (or below) played
    /// at the set level (0 dB) while 0.001 N·s was cut by 40 dB, and with a reference speed below
    /// `min_normal_speed_m_s` the approach-speed gate refused the curve, so every break played at
    /// 0 dB whatever its magnitude.
    #[test]
    fn a_break_level_follows_its_magnitude_whatever_the_minimums() {
        let mut zero_floor = def();
        zero_floor.curves.min_impulse_ns = 0.0;
        zero_floor.validate().expect("valid");
        let mut bank = ImpactBank::new(zero_floor);
        let tiny = bank
            .plan_break("wood", 0.001, Vec3::ZERO, 1)
            .expect("a break")
            .curve_db;
        for magnitude in [0.0f32, -5.0] {
            let play = bank
                .plan_break("wood", magnitude, Vec3::ZERO, 1)
                .expect("a break");
            assert!(
                play.curve_db <= tiny,
                "a break of {magnitude} N·s is not louder than one of 0.001 ({tiny} dB), got {} dB",
                play.curve_db
            );
        }

        let mut fast_gate = def();
        fast_gate.curves.min_normal_speed_m_s = 6.0; // above the 5 m/s reference speed
        fast_gate.validate().expect("valid");
        let mut bank = ImpactBank::new(fast_gate);
        let soft = bank
            .plan_break("wood", 0.5, Vec3::ZERO, 1)
            .expect("a break")
            .curve_db;
        let reference = bank
            .plan_break("wood", 10.0, Vec3::ZERO, 1)
            .expect("a break")
            .curve_db;
        assert!(
            soft < reference - 1.0,
            "a 0.5 N·s break is quieter than the 10 N·s reference: {soft} dB vs {reference} dB"
        );
    }

    /// The per-body variant history is keyed by entity bits and was never pruned: every body that
    /// ever hit something kept an entry for the session (debris, respawns with a new generation).
    /// It must stay bounded, and a live body must still not repeat a variant back to back.
    #[test]
    fn the_variant_history_stays_bounded_over_many_bodies() {
        let mut bank = ImpactBank::new(def());
        for body in 0..20_000u64 {
            let hit = ImpactContact {
                body,
                ..contact("wood", "stone", 5.0, 5.0)
            };
            bank.plan_impact(&hit).expect("plays");
        }
        assert!(
            bank.history.len() <= 4_096,
            "20 000 bodies leave {} history entries",
            bank.history.len()
        );
        let first = bank
            .plan_impact(&contact("wood", "stone", 5.0, 5.0))
            .expect("plays");
        let second = bank
            .plan_impact(&contact("wood", "stone", 5.0, 5.0))
            .expect("plays");
        assert_ne!(first.variant, second.variant, "no back-to-back repeat");
    }
}
