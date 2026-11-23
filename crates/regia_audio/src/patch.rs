// SPDX-License-Identifier: MPL-2.0
//! Parametric patches: sound layers driven by game-state parameters (`docs/specs/16-audio.md`
//! § 3a). **Format version 2** (`docs/formats/audio-patch-v2.md`); version 1 files
//! (`docs/formats/audio-patch-v1.md`, frozen) still load through the `v1 → v2` migrator
//! (ADR-7226).
//!
//! An [`AudioPatchDef`] is the RON asset: parameters (with smoothing times) and layers. A layer
//! is a looping sound whose volume, pitch and filter cutoff follow polylines of the parameters,
//! and optionally a **loop set**: several loop sounds crossfaded by one parameter (an engine
//! whose sample set slides from idle to redline with speed). Parameter changes are slewed on the
//! game thread and pushed into kira as short tweens, so the audio thread always interpolates —
//! no zipper noise (proven by the sweep test in `tests/acceptance.rs`).
//!
//! Version 2 adds two layer kinds and one parameter source:
//! - a **one-shot layer** ([`PatchOneShotDef`]) plays a sound, round-robin from a list, when a
//!   parameter crosses a threshold or a step, or when the game triggers it by name;
//! - a **granular layer** ([`PatchGranularDef`]) plays overlapping Hann-windowed grains read
//!   from a source sound at a parameter-driven position and density
//!   ([`crate::granular`]);
//! - a parameter may be **bound to a reflected field** ([`PatchBindingDef`]) of a component on
//!   the entity carrying [`crate::AudioPatchSource`]; the binding system sets it every frame.

use std::collections::HashMap;

use regia_core::version::{self, VersionStep};
use serde::{Deserialize, Serialize};

use crate::error::AudioError;
use crate::instance::SoundInstance;
use crate::loopgen::{LoopGenCache, is_loopgen_path};
use crate::sound::SoundAsset;
use crate::spatial::polyline;

/// The patch format version this engine reads and writes (v2: one-shot and granular layers,
/// reflected-field bindings; ADR-7226). Version 1 files migrate on load.
pub const PATCH_VERSION: u32 = 2;

/// A parameter bound to a reflected field (format v2): every frame the binding system reads
/// `component.field` on the entity carrying [`crate::AudioPatchSource`] and sets the parameter's
/// target to `value * scale + offset` (then clamped to the parameter's range and slewed as usual).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatchBindingDef {
    /// Short type name of a registered, reflected component (`"Vehicle"`).
    pub component: String,
    /// Reflect path of the field inside the component (`"speed"`, `"velocity.x"`). The leaf
    /// may be `f32`, `f64`, `i32`, `u32`, `i64`, `u64`, `u8`, `u16`, `i16` or `bool` (0 or 1).
    pub field: String,
    /// Multiplier applied to the field value.
    #[serde(default = "one_f32")]
    pub scale: f32,
    /// Offset added after the multiplier.
    #[serde(default)]
    pub offset: f32,
}

/// A parameter declaration: name, range and smoothing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatchParamDef {
    /// Parameter name (`"speed"`, `"throttle"`).
    pub name: String,
    /// Minimum value.
    pub min: f32,
    /// Maximum value.
    pub max: f32,
    /// Seconds the smoothed value takes to reach a new target (slew).
    #[serde(default = "default_smoothing")]
    pub smoothing_secs: f32,
    /// Format v2: a reflected field that drives this parameter (`None` = set from code only).
    #[serde(default)]
    pub binding: Option<PatchBindingDef>,
}

impl Default for PatchParamDef {
    /// An unnamed `0..1` parameter with the format's default smoothing and no binding.
    fn default() -> Self {
        Self {
            name: String::new(),
            min: 0.0,
            max: 1.0,
            smoothing_secs: default_smoothing(),
            binding: None,
        }
    }
}

fn default_smoothing() -> f32 {
    0.1
}

/// When a one-shot layer fires (format v2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PatchTriggerDef {
    /// Fires when the smoothed `parameter` rises from below `threshold` to at or above it.
    Rising {
        /// The parameter watched.
        parameter: String,
        /// The level crossed.
        threshold: f32,
    },
    /// Fires when the smoothed `parameter` falls from above `threshold` to at or below it.
    Falling {
        /// The parameter watched.
        parameter: String,
        /// The level crossed.
        threshold: f32,
    },
    /// Fires once for every multiple of `step` the smoothed `parameter` crosses, in either
    /// direction (a click per metre of travel, a tick per gear). At most
    /// [`MAX_FIRES_PER_TICK`] per tick.
    Every {
        /// The parameter watched.
        parameter: String,
        /// The spacing of the levels; must be positive.
        step: f32,
    },
    /// Fires only when the game (or an agent) triggers the layer by name
    /// (`Audio::patch_trigger`).
    Manual,
}

impl PatchTriggerDef {
    /// The watched parameter (`None` for [`PatchTriggerDef::Manual`]).
    pub fn parameter(&self) -> Option<&str> {
        match self {
            Self::Rising { parameter, .. }
            | Self::Falling { parameter, .. }
            | Self::Every { parameter, .. } => Some(parameter),
            Self::Manual => None,
        }
    }

    /// How many times the trigger fires when the watched value moves from `from` to `to`
    /// (pure; capped at [`MAX_FIRES_PER_TICK`]). Non-finite values never fire.
    pub fn fires(&self, from: f32, to: f32) -> u32 {
        if !from.is_finite() || !to.is_finite() {
            return 0;
        }
        match self {
            Self::Rising { threshold, .. } => u32::from(from < *threshold && to >= *threshold),
            Self::Falling { threshold, .. } => u32::from(from > *threshold && to <= *threshold),
            Self::Every { step, .. } => {
                if !(step.is_finite() && *step > 0.0) {
                    return 0;
                }
                let a = (f64::from(from) / f64::from(*step)).floor();
                let b = (f64::from(to) / f64::from(*step)).floor();
                ((a - b).abs().min(f64::from(MAX_FIRES_PER_TICK))) as u32
            }
            Self::Manual => 0,
        }
    }
}

/// The most one-shots one layer fires in one tick (a parameter jumping across many `Every`
/// steps at once does not start a burst of identical voices on the same sample).
pub const MAX_FIRES_PER_TICK: u32 = 4;

/// A one-shot layer (format v2): plays `sounds` in turn (round-robin, so a render is
/// reproducible) when `trigger` fires. Each shot starts at the layer's current volume and pitch
/// (base `volume_db` plus the layer curves) and plays through the layer's filter when it has
/// `filter_curves`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatchOneShotDef {
    /// Paths of the sounds, played in order and wrapping. At least one.
    pub sounds: Vec<String>,
    /// When the layer fires.
    pub trigger: PatchTriggerDef,
    /// Minimum seconds between two shots of this layer (a shot inside the window is skipped).
    #[serde(default)]
    pub cooldown_secs: f32,
}

/// A granular layer (format v2): overlapping Hann-windowed grains read from `sound`
/// ([`crate::granular`]). The layer's `volume_db` and `volume_curves` set the output gain, its
/// `pitch_curves` the grains' playback rate and its `filter_curves` a low-pass, as for a loop.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatchGranularDef {
    /// Path of the source sound grains are read from (treated as looping).
    pub sound: String,
    /// Grain length in seconds (> 0).
    #[serde(default = "default_grain_secs")]
    pub grain_secs: f32,
    /// Grains started per second (> 0). With `grain_secs * density_hz = 2` the Hann grains
    /// overlap by half and sum to a constant (a steady source plays back unchanged).
    #[serde(default = "default_density_hz")]
    pub density_hz: f32,
    /// Where grains start in the source, `0..1` of its length.
    #[serde(default)]
    pub position: f32,
    /// Position curves: outputs added to `position` (the sum is clamped to `0..1`).
    #[serde(default)]
    pub position_curves: Vec<PatchCurveDef>,
    /// Density curves: ratios multiplied into `density_hz`.
    #[serde(default)]
    pub density_curves: Vec<PatchCurveDef>,
    /// Random start offset per grain, `± position_jitter` of the source length (`0` = none).
    #[serde(default)]
    pub position_jitter: f32,
    /// Seed of the jitter sequence (a fixed seed renders the same grains every time).
    #[serde(default)]
    pub seed: u64,
}

fn default_grain_secs() -> f32 {
    0.05
}

fn default_density_hz() -> f32 {
    40.0
}

/// One key of a crossfaded loop set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatchKeyDef {
    /// The parameter value this key sits at.
    pub value: f32,
    /// Path of the loop sound for this key.
    pub sound: String,
    /// Gain of this key in dB at its centre.
    #[serde(default)]
    pub gain_db: f32,
    /// Pitch ratio of this key (usually 1; the parameter curve can add more).
    #[serde(default = "one_f32")]
    pub pitch: f32,
}

fn one_f32() -> f32 {
    1.0
}

/// A parameter-driven polyline `(parameter value, output)`; the output meaning depends on where
/// the curve is attached (dB, pitch ratio, or Hz).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PatchCurveDef {
    /// The parameter driving the curve.
    pub parameter: String,
    /// The polyline.
    pub points: Vec<(f32, f32)>,
}

/// One layer of a patch. Exactly one of `sound`, `keys`, `one_shot` (v2) and `granular` (v2)
/// names what it plays.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PatchLayerDef {
    /// Layer name.
    pub name: String,
    /// Path of the layer's loop sound (used when the layer has no `keys`).
    #[serde(default)]
    pub sound: Option<String>,
    /// Crossfaded loop set keyed by `parameter` (used instead of `sound` when non-empty).
    #[serde(default)]
    pub keys: Vec<PatchKeyDef>,
    /// Which parameter drives the key crossfade.
    #[serde(default)]
    pub parameter: Option<String>,
    /// Base volume of the layer in dB.
    #[serde(default)]
    pub volume_db: f32,
    /// Volume curves (dB offsets) per parameter.
    #[serde(default)]
    pub volume_curves: Vec<PatchCurveDef>,
    /// Pitch curves (ratios, multiplied) per parameter.
    #[serde(default)]
    pub pitch_curves: Vec<PatchCurveDef>,
    /// Filter cutoff curves (Hz) per parameter; the layer gets a low-pass.
    #[serde(default)]
    pub filter_curves: Vec<PatchCurveDef>,
    /// Format v2: a one-shot layer (instead of `sound`/`keys`).
    #[serde(default)]
    pub one_shot: Option<PatchOneShotDef>,
    /// Format v2: a granular layer (instead of `sound`/`keys`).
    #[serde(default)]
    pub granular: Option<PatchGranularDef>,
}

/// What a layer plays (derived from which of its fields are set).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchLayerKind {
    /// One looping `sound`.
    Loop,
    /// A crossfaded loop set (`keys`).
    Keys,
    /// A one-shot layer (`one_shot`).
    OneShot,
    /// A granular layer (`granular`).
    Granular,
}

impl PatchLayerKind {
    /// The kind's name as the agent protocol and the docs spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loop => "loop",
            Self::Keys => "keys",
            Self::OneShot => "one_shot",
            Self::Granular => "granular",
        }
    }
}

impl PatchLayerDef {
    /// The layer's kind, or an error naming the conflict when more than one of `sound`, `keys`,
    /// `one_shot` and `granular` is set (or none is).
    pub fn kind(&self) -> Result<PatchLayerKind, AudioError> {
        let mut kinds = Vec::with_capacity(1);
        if self.sound.is_some() && self.keys.is_empty() {
            kinds.push(PatchLayerKind::Loop);
        }
        if !self.keys.is_empty() {
            kinds.push(PatchLayerKind::Keys);
        }
        if self.one_shot.is_some() {
            kinds.push(PatchLayerKind::OneShot);
        }
        if self.granular.is_some() {
            kinds.push(PatchLayerKind::Granular);
        }
        match kinds.as_slice() {
            [kind] => Ok(*kind),
            [] => Err(AudioError::Invalid(format!(
                "patch layer `{}` has none of `sound`, `keys`, `one_shot`, `granular`",
                self.name
            ))),
            _ => Err(AudioError::Invalid(format!(
                "patch layer `{}` sets more than one of `sound`/`keys`, `one_shot`, `granular`",
                self.name
            ))),
        }
    }
}

/// The RON form of a patch (`.audiopatch.ron`), version 2 (version 1 files migrate on load).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioPatchDef {
    /// Format version: [`PATCH_VERSION`] after loading; 1 is migrated.
    pub version: u32,
    /// Patch name.
    pub name: String,
    /// Bus the patch plays on (`"Sfx"`).
    #[serde(default)]
    pub bus: Option<String>,
    /// Parameters.
    pub parameters: Vec<PatchParamDef>,
    /// Layers.
    pub layers: Vec<PatchLayerDef>,
}

impl AudioPatchDef {
    /// Parses a patch definition from RON text, checking the version and running registered
    /// migrators for older versions.
    ///
    /// The `version` field is peeked before the full deserialise, so a *newer* version is rejected
    /// even when its body no longer matches the current struct.
    pub fn from_ron(text: &str) -> Result<Self, AudioError> {
        let version =
            regia_core::version::peek_ron_version(text).map_err(|e| AudioError::load("patch", e))?;
        if version > PATCH_VERSION {
            return Err(AudioError::UnsupportedPatchVersion(version));
        }
        let mut def: Self = ron::from_str(text).map_err(|e| AudioError::load("patch", e))?;
        def.migrate_to_latest()?;
        Ok(def)
    }

    /// Reject a newer version, or run the registered migrator chain to bring an older file up to
    /// [`PATCH_VERSION`] (`docs/specs/12-assets.md` § 4a).
    pub fn migrate_to_latest(&mut self) -> Result<u32, AudioError> {
        if self.version > PATCH_VERSION {
            return Err(AudioError::UnsupportedPatchVersion(self.version));
        }
        if self.version < PATCH_VERSION {
            let to = version::migrate(self, self.version, builtin_migrations(), PATCH_VERSION)
                .map_err(|e| AudioError::Migration(e.to_string()))?;
            self.version = to;
        }
        Ok(self.version)
    }

    /// True when the definition uses a field that only format v2 has (a one-shot or granular
    /// layer, or a parameter binding).
    pub fn uses_v2_fields(&self) -> bool {
        self.parameters.iter().any(|p| p.binding.is_some())
            || self
                .layers
                .iter()
                .any(|l| l.one_shot.is_some() || l.granular.is_some())
    }

    /// Writes the definition as pretty RON (the form `regia upgrade-project` and the editor save).
    pub fn to_ron(&self) -> Result<String, AudioError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty()).map_err(|e| AudioError::load("patch", e))
    }
}

/// `.audiopatch.ron` v1 → v2 (ADR-7226). Every v2 addition is optional and defaults to "absent",
/// so a v1 file keeps its meaning unchanged: the step checks that the file does not already use
/// a v2-only field (that would be a v2 file with a wrong version number) and stamps the version.
fn migrate_patch_v1_to_v2(def: &mut AudioPatchDef) -> Result<(), String> {
    if def.uses_v2_fields() {
        return Err(
            "a version 1 patch uses a version 2 field (`binding`, `one_shot` or `granular`); set `version: 2`"
                .to_string(),
        );
    }
    def.version = 2;
    Ok(())
}

static PATCH_MIGRATIONS: [VersionStep<AudioPatchDef>; 1] = [VersionStep {
    from: 1,
    to: 2,
    migrate: migrate_patch_v1_to_v2,
}];

/// The built-in migration chain for `.audiopatch.ron`: `v1 → v2` (ADR-7226). `regia
/// upgrade-project` runs the same chain.
pub fn builtin_migrations() -> &'static [VersionStep<AudioPatchDef>] {
    &PATCH_MIGRATIONS
}

/// The versions a built-in migration step leaves (`from` side), for `cargo xtask check-formats`
/// to verify a version bump ships with a migrator from the previous version.
pub fn patch_migration_sources() -> Vec<u32> {
    builtin_migrations().iter().map(|s| s.from).collect()
}

/// The serde field-list fingerprint of the `.audiopatch.ron` format (hashed by
/// `cargo xtask check-formats`).
///
/// The root struct *and* every nested struct are listed, so a shape change anywhere in the tree
/// changes the hash.
pub fn patch_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<AudioPatchDef>(),
        regia_core::schema::serde_field_names::<PatchParamDef>(),
        regia_core::schema::serde_field_names::<PatchKeyDef>(),
        regia_core::schema::serde_field_names::<PatchCurveDef>(),
        regia_core::schema::serde_field_names::<PatchLayerDef>(),
        regia_core::schema::serde_field_names::<PatchBindingDef>(),
        regia_core::schema::serde_field_names::<PatchOneShotDef>(),
        regia_core::schema::serde_variant_names::<PatchTriggerDef>(),
        regia_core::schema::serde_variant_shapes::<PatchTriggerDef>(),
        regia_core::schema::serde_field_names::<PatchGranularDef>(),
    ])
}

/// A parameter with its smoothed value (linear slew toward the target).
#[derive(Clone, Debug, PartialEq)]
pub struct SmoothedParam {
    /// The declared parameter.
    pub def: PatchParamDef,
    /// Current smoothed value.
    pub value: f32,
    /// Last set target.
    pub target: f32,
}

impl SmoothedParam {
    pub(crate) fn new(def: PatchParamDef) -> Self {
        let value = def.min;
        Self {
            def,
            value,
            target: value,
        }
    }

    /// Sets the target (clamped to the declared range).
    pub fn set_target(&mut self, target: f32) {
        // `f32::clamp` panics on inverted or NaN bounds, and an authored file can carry either.
        let (lo, hi) = if self.def.min <= self.def.max {
            (self.def.min, self.def.max)
        } else {
            (self.def.max, self.def.min)
        };
        if target.is_nan() || lo.is_nan() || hi.is_nan() {
            return;
        }
        self.target = target.clamp(lo, hi);
    }

    /// Advances the smoothing by `dt`; returns true when the value changed.
    pub fn tick(&mut self, dt: f64) -> bool {
        // A NaN step would make the slew NaN and poison the value for good; a negative one would
        // run away from the target. Neither moves the value.
        if dt.is_nan() || dt < 0.0 {
            return false;
        }
        let remaining = (self.target - self.value).abs();
        if remaining < 1e-6 {
            return false;
        }
        let smoothing = self.def.smoothing_secs.max(1e-3) as f64;
        // Linear slew: the remaining distance is covered in `smoothing` seconds.
        let step = (f64::from(remaining) / smoothing * dt) as f32;
        if step >= remaining {
            self.value = self.target;
        } else if self.target > self.value {
            self.value += step;
        } else {
            self.value -= step;
        }
        true
    }

    /// Normalised position of the value in the declared range (for key crossfades).
    pub fn normalised(&self) -> f32 {
        let span = self.def.max - self.def.min;
        if span.abs() < 1e-9 {
            return 0.0;
        }
        ((self.value - self.def.min) / span).clamp(0.0, 1.0)
    }
}

/// A resolved patch: definitions plus loaded sounds, ready to spawn.
#[derive(Clone)]
pub struct AudioPatch {
    /// The definition this was built from.
    pub def: AudioPatchDef,
    /// Loaded sound per layer: the loop of a `sound` layer, the source of a `granular` layer.
    pub layer_sounds: HashMap<String, SoundAsset>,
    /// Loaded sounds per `(layer, index)`: the loop of each key of a `keys` layer, or each sound
    /// of a `one_shot` layer.
    pub key_sounds: HashMap<(String, usize), SoundAsset>,
}

/// Rejects a non-finite or negative value (or a non-positive one when `positive`) of a v2
/// layer field.
fn check_finite(layer: &str, what: &str, value: f32, positive: bool) -> Result<(), AudioError> {
    if !value.is_finite() || (positive && value <= 0.0) || value < 0.0 {
        return Err(AudioError::Invalid(format!(
            "patch layer `{layer}`: `{what}` must be a finite{} number, got {value}",
            if positive { " positive" } else { " non-negative" }
        )));
    }
    Ok(())
}

impl AudioPatch {
    /// Builds a patch from its definition, resolving sound paths with `resolve`. An older
    /// definition (version 1) is migrated first; each layer must name exactly one of `sound`,
    /// `keys`, `one_shot` and `granular`.
    pub fn from_def(
        mut def: AudioPatchDef,
        resolve: impl Fn(&str) -> Result<SoundAsset, AudioError>,
    ) -> Result<Self, AudioError> {
        def.migrate_to_latest()?;
        let declared = |name: &str| def.parameters.iter().any(|p| p.name == name);
        for param in &def.parameters {
            if let Some(binding) = &param.binding
                && (binding.component.is_empty() || binding.field.is_empty())
            {
                return Err(AudioError::Invalid(format!(
                    "patch parameter `{}`: a binding needs a `component` and a `field`",
                    param.name
                )));
            }
        }
        let mut layer_sounds = HashMap::new();
        let mut key_sounds = HashMap::new();
        for layer in &def.layers {
            match layer.kind()? {
                PatchLayerKind::Loop => {
                    if let Some(path) = &layer.sound {
                        layer_sounds.insert(layer.name.clone(), resolve(path)?);
                    }
                }
                PatchLayerKind::Keys => {
                    let driven = layer.parameter.as_deref().is_some_and(declared);
                    if !driven {
                        return Err(AudioError::UnknownEntry(format!(
                            "patch layer `{}`: key parameter {:?} is not a declared parameter",
                            layer.name, layer.parameter
                        )));
                    }
                    for (index, key) in layer.keys.iter().enumerate() {
                        key_sounds.insert((layer.name.clone(), index), resolve(&key.sound)?);
                    }
                }
                PatchLayerKind::OneShot => {
                    let Some(shot) = &layer.one_shot else {
                        continue;
                    };
                    if shot.sounds.is_empty() {
                        return Err(AudioError::Invalid(format!(
                            "patch layer `{}`: a one-shot layer needs at least one sound",
                            layer.name
                        )));
                    }
                    check_finite(&layer.name, "cooldown_secs", shot.cooldown_secs, false)?;
                    if let Some(parameter) = shot.trigger.parameter()
                        && !declared(parameter)
                    {
                        return Err(AudioError::UnknownEntry(format!(
                            "patch layer `{}`: trigger parameter `{parameter}` is not a declared parameter",
                            layer.name
                        )));
                    }
                    match &shot.trigger {
                        PatchTriggerDef::Rising { threshold, .. }
                        | PatchTriggerDef::Falling { threshold, .. }
                            if !threshold.is_finite() =>
                        {
                            return Err(AudioError::Invalid(format!(
                                "patch layer `{}`: the trigger threshold must be finite",
                                layer.name
                            )));
                        }
                        PatchTriggerDef::Every { step, .. } => {
                            check_finite(&layer.name, "step", *step, true)?;
                        }
                        _ => {}
                    }
                    for (index, path) in shot.sounds.iter().enumerate() {
                        key_sounds.insert((layer.name.clone(), index), resolve(path)?);
                    }
                }
                PatchLayerKind::Granular => {
                    let Some(grains) = &layer.granular else {
                        continue;
                    };
                    check_finite(&layer.name, "grain_secs", grains.grain_secs, true)?;
                    check_finite(&layer.name, "density_hz", grains.density_hz, true)?;
                    check_finite(&layer.name, "position_jitter", grains.position_jitter, false)?;
                    if !grains.position.is_finite() {
                        return Err(AudioError::Invalid(format!(
                            "patch layer `{}`: `position` must be finite",
                            layer.name
                        )));
                    }
                    for curve in grains.position_curves.iter().chain(&grains.density_curves) {
                        if !declared(&curve.parameter) {
                            return Err(AudioError::UnknownEntry(format!(
                                "patch layer `{}`: curve parameter `{}` is not a declared parameter",
                                layer.name, curve.parameter
                            )));
                        }
                    }
                    layer_sounds.insert(layer.name.clone(), resolve(&grains.sound)?);
                }
            }
        }
        Ok(Self {
            def,
            layer_sounds,
            key_sounds,
        })
    }

    /// Like [`Self::from_def`], but a `sound` path ending in `.loopgen.ron` is a procedural loop:
    /// `read` returns the file's text and `cache` renders it (identical text renders once). Every
    /// other path goes to `resolve`.
    pub fn from_def_with_loopgen(
        def: AudioPatchDef,
        cache: &LoopGenCache,
        read: impl Fn(&str) -> Result<String, AudioError>,
        resolve: impl Fn(&str) -> Result<SoundAsset, AudioError>,
    ) -> Result<Self, AudioError> {
        Self::from_def(def, |path| {
            if is_loopgen_path(path) {
                cache.sound_asset(&read(path)?)
            } else {
                resolve(path)
            }
        })
    }

    /// The patch's name.
    pub fn name(&self) -> &str {
        &self.def.name
    }
}

/// The weights of a key crossfade at a parameter value: `(key index, amplitude 0..1)` pairs for
/// the (at most two) keys audible at that value. The fade is equal-power (the squared weights sum
/// to 1). Pure; unit-tested. The frame path uses [`crossfade_weights_into`].
#[cfg(test)]
pub fn crossfade_weights(keys: &[PatchKeyDef], value: f32) -> Vec<(usize, f32)> {
    let mut order = Vec::new();
    let mut out = Vec::new();
    crossfade_weights_into(keys, value, &mut order, &mut out);
    out
}

/// Key-crossfade weights into reused buffers. `order` is key indices sorted by value; `out`
/// receives at most two weights. Both keep their capacity across calls. The fade is equal-power.
pub fn crossfade_weights_into(
    keys: &[PatchKeyDef],
    value: f32,
    order: &mut Vec<usize>,
    out: &mut Vec<(usize, f32)>,
) {
    out.clear();
    if keys.is_empty() {
        return;
    }
    order.clear();
    order.extend(0..keys.len());
    order.sort_by(|&a, &b| keys[a].value.total_cmp(&keys[b].value));
    let first = order[0];
    if order.len() == 1 || value <= keys[first].value {
        out.push((first, 1.0));
        return;
    }
    let last = order[order.len() - 1];
    if value >= keys[last].value {
        out.push((last, 1.0));
        return;
    }
    for pair in order.windows(2) {
        let (i0, i1) = (pair[0], pair[1]);
        if value <= keys[i1].value {
            let span = keys[i1].value - keys[i0].value;
            let t = if span.abs() < 1e-9 {
                0.5
            } else {
                ((value - keys[i0].value) / span).clamp(0.0, 1.0)
            };
            // Equal power: the squares sum to 1, so the loop does not dip between keys.
            let angle = t * std::f32::consts::FRAC_PI_2;
            out.push((i0, angle.cos()));
            out.push((i1, angle.sin()));
            return;
        }
    }
    out.push((last, 1.0));
}

/// One layer's computed outputs for the current parameter values.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct LayerOutputs {
    /// Volume in dB.
    pub volume_db: f32,
    /// Pitch ratio.
    pub pitch: f32,
    /// Filter cutoff in Hz (None = no filter).
    pub cutoff_hz: Option<f64>,
}

/// The grain controls of a granular layer at the current parameter values (pure): the start
/// position (`0..1`, `position` plus the position curves, clamped) and the density in grains per
/// second (`density_hz` times the density curves, floored at 0.1 Hz).
pub fn granular_controls(grains: &PatchGranularDef, params: &HashMap<String, SmoothedParam>) -> (f32, f32) {
    let mut position = grains.position;
    for curve in &grains.position_curves {
        if let Some(param) = params.get(&curve.parameter) {
            position += polyline(&curve.points, param.value);
        }
    }
    let mut density = grains.density_hz;
    for curve in &grains.density_curves {
        if let Some(param) = params.get(&curve.parameter) {
            density *= polyline(&curve.points, param.value).max(0.0);
        }
    }
    let position = if position.is_finite() {
        position.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let density = if density.is_finite() {
        density.max(0.1)
    } else {
        0.1
    };
    (position, density)
}

/// Computes one layer's volume/pitch/cutoff from smoothed parameters (pure).
pub fn layer_outputs(layer: &PatchLayerDef, params: &HashMap<String, SmoothedParam>) -> LayerOutputs {
    let mut out = LayerOutputs {
        volume_db: layer.volume_db,
        pitch: 1.0,
        cutoff_hz: None,
    };
    for curve in &layer.volume_curves {
        if let Some(param) = params.get(&curve.parameter) {
            out.volume_db += polyline(&curve.points, param.value);
        }
    }
    for curve in &layer.pitch_curves {
        if let Some(param) = params.get(&curve.parameter) {
            out.pitch *= polyline(&curve.points, param.value).max(0.0);
        }
    }
    let mut cutoff: Option<f32> = None;
    for curve in &layer.filter_curves {
        if let Some(param) = params.get(&curve.parameter) {
            let value = polyline(&curve.points, param.value).max(20.0);
            cutoff = Some(match cutoff {
                Some(existing) => existing.min(value),
                None => value,
            });
        }
    }
    out.cutoff_hz = cutoff.map(f64::from);
    out
}

/// One playing loop of a layer (a `sound` layer has one; a key set has one per key).
pub struct PatchLoop {
    /// The playing instance.
    pub instance: SoundInstance,
    /// Asset volume and loudness offset, also applied to live parameter changes.
    pub volume_offset_db: f32,
    /// Asset pitch multiplier, also applied to live parameter changes.
    pub pitch_multiplier: f32,
    /// The last volume applied, dB.
    pub applied_db: f32,
    /// The last pitch applied.
    pub applied_pitch: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::gain_to_db;

    fn param(name: &str, min: f32, max: f32, smoothing: f32) -> SmoothedParam {
        SmoothedParam::new(PatchParamDef {
            name: name.into(),
            min,
            max,
            smoothing_secs: smoothing,
            binding: None,
        })
    }

    #[test]
    fn set_target_survives_inverted_and_nan_bounds() {
        let mut inverted = param("a", 10.0, 0.0, 0.1);
        inverted.set_target(5.0);
        assert_eq!(inverted.target, 5.0);
        inverted.set_target(50.0);
        assert_eq!(inverted.target, 10.0);
        let mut nan = param("b", f32::NAN, 1.0, 0.1);
        let before = nan.target;
        nan.set_target(0.5);
        assert!(nan.target.is_nan() == before.is_nan());
        let mut ok = param("c", 0.0, 1.0, 0.1);
        ok.set_target(0.25);
        ok.set_target(f32::NAN);
        assert_eq!(ok.target, 0.25);
    }

    #[test]
    fn smoothing_converges_without_overshoot() {
        let mut p = param("speed", 0.0, 100.0, 0.5);
        p.set_target(150.0); // clamped to 100
        assert_eq!(p.target, 100.0);
        let mut saw = Vec::new();
        for _ in 0..200 {
            p.tick(1.0 / 60.0);
            saw.push(p.value);
        }
        assert!((p.value - 100.0).abs() < 0.5, "converges, got {}", p.value);
        for pair in saw.windows(2) {
            assert!(pair[1] >= pair[0] - 1e-9, "monotonic rise");
            assert!(pair[1] <= 100.0 + 1e-9, "no overshoot");
        }
    }

    #[test]
    fn crossfade_weights_bracket_and_clamp() {
        let keys = vec![
            PatchKeyDef {
                value: 0.0,
                sound: "idle".into(),
                gain_db: 0.0,
                pitch: 1.0,
            },
            PatchKeyDef {
                value: 0.5,
                sound: "mid".into(),
                gain_db: 0.0,
                pitch: 1.0,
            },
            PatchKeyDef {
                value: 1.0,
                sound: "fast".into(),
                gain_db: 0.0,
                pitch: 1.0,
            },
        ];
        assert_eq!(crossfade_weights(&keys, -1.0), vec![(0, 1.0)]);
        assert_eq!(crossfade_weights(&keys, 2.0), vec![(2, 1.0)]);
        let mid = crossfade_weights(&keys, 0.25);
        assert_eq!(mid.len(), 2);
        // Equal power at the midpoint: each key sits at 1/sqrt(2) (about -3 dB).
        let half_power = std::f32::consts::FRAC_1_SQRT_2;
        assert!((mid[0].1 - half_power).abs() < 1e-6 && (mid[1].1 - half_power).abs() < 1e-6);
        let at_0 = crossfade_weights(&keys, 0.0);
        assert_eq!(at_0, vec![(0, 1.0)], "exact key value is its own full weight");
        let empty = crossfade_weights(&[], 0.3);
        assert!(empty.is_empty());
    }

    #[test]
    fn layer_outputs_combine_curves() {
        let layer = PatchLayerDef {
            name: "whine".into(),
            sound: Some("whine.ogg".into()),
            keys: vec![],
            parameter: None,
            volume_db: -12.0,
            volume_curves: vec![PatchCurveDef {
                parameter: "throttle".into(),
                points: vec![(0.0, -20.0), (1.0, 0.0)],
            }],
            pitch_curves: vec![PatchCurveDef {
                parameter: "throttle".into(),
                points: vec![(0.0, 0.5), (1.0, 1.5)],
            }],
            filter_curves: vec![PatchCurveDef {
                parameter: "speed".into(),
                points: vec![(0.0, 500.0), (100.0, 18_000.0)],
            }],
            one_shot: None,
            granular: None,
        };
        let mut params = HashMap::new();
        params.insert("throttle".to_string(), param("throttle", 0.0, 1.0, 0.1));
        params.insert("speed".to_string(), param("speed", 0.0, 100.0, 0.1));
        params.get_mut("throttle").expect("inserted").value = 0.5;
        params.get_mut("speed").expect("inserted").value = 50.0;

        let out = layer_outputs(&layer, &params);
        assert!((out.volume_db - (-22.0)).abs() < 1e-6, "-12 base + -10 curve");
        assert!(
            (out.pitch - 1.0).abs() < 1e-6,
            "0.5·1.5 = ... interp at 0.5 → 1.0"
        );
        assert_eq!(out.cutoff_hz, Some(9_250.0));
    }

    #[test]
    fn version_check_rejects_other_versions() {
        let text = "(version: 3, name: \"x\", parameters: [], layers: [])";
        assert!(matches!(
            AudioPatchDef::from_ron(text),
            Err(AudioError::UnsupportedPatchVersion(3))
        ));
    }

    #[test]
    fn key_crossfade_holds_power_across_the_range() {
        let keys: Vec<PatchKeyDef> = [0.0f32, 1.0]
            .into_iter()
            .map(|value| PatchKeyDef {
                value,
                sound: "k".into(),
                gain_db: 0.0,
                pitch: 1.0,
            })
            .collect();
        for step in 1..20 {
            let weights = crossfade_weights(&keys, step as f32 / 20.0);
            let power: f32 = weights.iter().map(|(_, w)| w * w).sum();
            assert!((power - 1.0).abs() < 1e-5, "step {step}: power {power}");
        }
        let mid = crossfade_weights(&keys, 0.5);
        assert!(
            (gain_to_db(mid[0].1) + 3.01).abs() < 0.02,
            "each key about -3 dB at the midpoint"
        );
    }

    #[test]
    fn keyed_layer_needs_a_declared_parameter() {
        let key = |value: f32| PatchKeyDef {
            value,
            sound: "k".into(),
            gain_db: 0.0,
            pitch: 1.0,
        };
        let layer = |parameter: Option<&str>| PatchLayerDef {
            name: "loops".into(),
            sound: None,
            keys: vec![key(0.0), key(1.0)],
            parameter: parameter.map(str::to_string),
            volume_db: 0.0,
            volume_curves: vec![],
            pitch_curves: vec![],
            filter_curves: vec![],
            one_shot: None,
            granular: None,
        };
        let def = |parameter: Option<&str>| AudioPatchDef {
            version: PATCH_VERSION,
            name: "p".into(),
            bus: None,
            parameters: vec![PatchParamDef {
                name: "speed".into(),
                min: 0.0,
                max: 1.0,
                smoothing_secs: 0.1,
                binding: None,
            }],
            layers: vec![layer(parameter)],
        };
        let resolve = |_: &str| Ok(crate::test_support::asset());
        assert!(AudioPatch::from_def(def(Some("speed")), resolve).is_ok());
        assert!(
            AudioPatch::from_def(def(Some("throttle")), resolve).is_err(),
            "undeclared parameter"
        );
        assert!(
            AudioPatch::from_def(def(None), resolve).is_err(),
            "no parameter at all"
        );
    }

    #[test]
    fn gain_to_db_pairs_with_crossfade_weights() {
        // Two keys at half weight each: both drop ~6 dB, summing back to ~1.
        let w = 0.5f32;
        let db = gain_to_db(w);
        assert!((db + 6.02).abs() < 0.01);
        assert!((gain_to_db(1.0)).abs() < 1e-9);
    }

    fn keys_at(values: &[f32]) -> Vec<PatchKeyDef> {
        values
            .iter()
            .map(|&value| PatchKeyDef {
                value,
                sound: "k".into(),
                gain_db: 0.0,
                pitch: 1.0,
            })
            .collect()
    }

    #[test]
    fn crossfade_of_no_keys_and_one_key() {
        assert!(crossfade_weights(&[], 0.5).is_empty());
        assert_eq!(crossfade_weights(&keys_at(&[3.0]), -9.0), vec![(0, 1.0)]);
        assert_eq!(crossfade_weights(&keys_at(&[3.0]), 99.0), vec![(0, 1.0)]);
    }

    #[test]
    fn crossfade_outside_the_keys_holds_the_end_key() {
        let keys = keys_at(&[0.0, 0.5, 1.0]);
        assert_eq!(crossfade_weights(&keys, -5.0), vec![(0, 1.0)]);
        assert_eq!(crossfade_weights(&keys, 5.0), vec![(2, 1.0)]);
        assert_eq!(
            crossfade_weights(&keys, 0.5).len(),
            2,
            "on a key: that key and its upper neighbour"
        );
    }

    #[test]
    fn crossfade_reports_authored_indices_when_the_keys_are_out_of_order() {
        // Authored order 1.0, 0.0, 0.5: the sorted neighbours of 0.25 are keys 1 (0.0) and 2 (0.5).
        let keys = keys_at(&[1.0, 0.0, 0.5]);
        let weights = crossfade_weights(&keys, 0.25);
        let indices: Vec<usize> = weights.iter().map(|w| w.0).collect();
        assert_eq!(indices, [1, 2]);
        let power: f32 = weights.iter().map(|(_, w)| w * w).sum();
        assert!((power - 1.0).abs() < 1e-5);
    }

    #[test]
    fn crossfade_with_duplicate_key_values_does_not_divide_by_zero() {
        let keys = keys_at(&[0.0, 0.5, 0.5, 1.0]);
        for value in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let weights = crossfade_weights(&keys, value);
            assert!(weights.iter().all(|(_, w)| w.is_finite()), "{value}: {weights:?}");
            let power: f32 = weights.iter().map(|(_, w)| w * w).sum();
            assert!((power - 1.0).abs() < 1e-5, "{value}: {weights:?}");
        }
    }

    #[test]
    fn crossfade_of_nan_is_finite() {
        let weights = crossfade_weights(&keys_at(&[0.0, 1.0]), f32::NAN);
        assert!(weights.iter().all(|(_, w)| w.is_finite()), "{weights:?}");
    }

    #[test]
    fn layer_outputs_skip_unknown_parameters_and_floor_the_cutoff() {
        let layer = PatchLayerDef {
            name: "l".into(),
            sound: Some("s".into()),
            keys: Vec::new(),
            parameter: None,
            volume_db: -3.0,
            volume_curves: vec![PatchCurveDef {
                parameter: "missing".into(),
                points: vec![(0.0, -50.0), (1.0, -50.0)],
            }],
            pitch_curves: vec![PatchCurveDef {
                parameter: "p".into(),
                points: vec![(0.0, -2.0), (1.0, -2.0)],
            }],
            filter_curves: vec![
                PatchCurveDef {
                    parameter: "p".into(),
                    points: vec![(0.0, 5.0), (1.0, 5.0)],
                },
                PatchCurveDef {
                    parameter: "p".into(),
                    points: vec![(0.0, 900.0), (1.0, 900.0)],
                },
            ],
            one_shot: None,
            granular: None,
        };
        let mut params = HashMap::new();
        params.insert("p".to_string(), param("p", 0.0, 1.0, 0.1));
        let out = layer_outputs(&layer, &params);
        assert_eq!(
            out.volume_db, -3.0,
            "a curve on an unknown parameter adds nothing"
        );
        assert_eq!(
            out.pitch, 0.0,
            "a negative pitch curve is floored at 0 (the caller floors it again)"
        );
        assert_eq!(
            out.cutoff_hz,
            Some(20.0),
            "the lowest of several filter curves, floored at 20 Hz"
        );
        let bare = layer_outputs(
            &PatchLayerDef {
                volume_curves: Vec::new(),
                pitch_curves: Vec::new(),
                filter_curves: Vec::new(),
                ..layer
            },
            &params,
        );
        assert_eq!((bare.pitch, bare.cutoff_hz), (1.0, None));
    }
}
