// SPDX-License-Identifier: MPL-2.0
//! Mixer buses (kira tracks): the layout tree, per-bus effects and sends (`docs/specs/16-audio.md`
//! § 1).
//!
//! A [`BusLayout`] is data (RON-serializable, `config/audio.ron`); the default tree is the one the
//! spec names: `Master → { Music, Sfx → { World, Ui }, Voice, Ambience }` plus a `Reverb` send bus
//! that `World` and `Ambience` feed. [`Buses`] is the live set of kira tracks built from a layout;
//! volumes can be retargeted with blend times at runtime, and low/high-pass cutoffs are tweenable.

use std::collections::HashMap;

use kira::{
    Decibels, Tween,
    effect::{
        compressor::CompressorBuilder,
        delay::DelayBuilder,
        distortion::{DistortionBuilder, DistortionKind},
        eq_filter::{EqFilterBuilder, EqFilterKind},
        filter::{FilterBuilder, FilterHandle, FilterMode},
        reverb::{ReverbBuilder, ReverbHandle},
    },
    track::{SendTrackBuilder, SendTrackHandle, TrackBuilder, TrackHandle},
};
use serde::{Deserialize, Serialize};

use crate::snapshot::AudioSnapshot;

/// One effect in a bus's chain. Parameters are the subset exposed to gameplay and the editor;
/// everything is tweenable where kira supports it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BusEffect {
    /// 3-band EQ (low/high shelves + mid bell).
    Eq {
        /// Low shelf corner in Hz.
        low_hz: f32,
        /// Low shelf gain in dB.
        low_db: f32,
        /// Mid bell centre in Hz.
        mid_hz: f32,
        /// Mid bell gain in dB.
        mid_db: f32,
        /// High shelf corner in Hz.
        high_hz: f32,
        /// High shelf gain in dB.
        high_db: f32,
        /// Q of the mid bell (> 0).
        quality: f32,
    },
    /// Low-pass filter.
    LowPass {
        /// Cutoff in Hz.
        cutoff_hz: f32,
        /// Resonance (> 0).
        resonance: f32,
    },
    /// High-pass filter.
    HighPass {
        /// Cutoff in Hz.
        cutoff_hz: f32,
        /// Resonance (> 0).
        resonance: f32,
    },
    /// Freeverb-style reverb.
    Reverb {
        /// Feedback 0..1.
        feedback: f32,
        /// High-frequency damping 0..1.
        damping: f32,
        /// Stereo width 0..1.
        stereo_width: f32,
        /// Dry/wet mix 0..1 (1 = wet only; send buses usually want 1).
        mix: f32,
    },
    /// Feedback delay.
    Delay {
        /// Delay time in seconds.
        delay_secs: f32,
        /// Feedback gain in dB.
        feedback_db: f32,
        /// Dry/wet mix 0..1.
        mix: f32,
    },
    /// Compressor.
    Compressor {
        /// Threshold in dB.
        threshold_db: f32,
        /// Ratio (> 0, 1 = no compression).
        ratio: f32,
        /// Attack time in seconds.
        attack_secs: f32,
        /// Release time in seconds.
        release_secs: f32,
        /// Makeup gain in dB.
        makeup_db: f32,
    },
    /// Distortion.
    Distortion {
        /// Drive in dB.
        drive_db: f32,
    },
    /// Convolution with an impulse response (ADR-2721, [`crate::ImpulseResponse`]): a measured
    /// room, a spring, a cabinet. The IR is one registered with
    /// [`crate::Audio::register_impulse_response`]; an unknown or refused IR passes only the dry
    /// part of the mix (counted, logged). The wet path is `max(predelay, 256 frames)` late.
    Convolution {
        /// The registered IR's name.
        ir: String,
        /// Dry/wet mix 0..1 (1 = wet only; send buses usually want 1).
        mix: f32,
        /// Predelay before the reverb, seconds (0..=0.5).
        #[serde(default)]
        predelay_secs: f32,
    },
    /// A bypassed effect (`audio-buses` v1, ADR-8105 (g), ADR-8354): the wrapped effect keeps its
    /// parameters in the chain but is not built, so the bus passes its signal through that slot
    /// unchanged. `Bypassed(Bypassed(_))` is refused by [`BusLayout::validate`].
    Bypassed(Box<BusEffect>),
}

/// The effect kinds a mixer can add to a chain, by their stable names (`eq`, `low_pass`,
/// `high_pass`, `reverb`, `delay`, `compressor`, `distortion`, `convolution`).
pub const BUS_EFFECT_KINDS: &[&str] = &[
    "eq",
    "low_pass",
    "high_pass",
    "reverb",
    "delay",
    "compressor",
    "distortion",
    "convolution",
];

/// Longest effect time handed to kira, seconds (keeps `Duration` conversion total).
const MAX_EFFECT_SECS: f32 = 3_600.0;

/// A `Duration` from a user-supplied seconds value: NaN, negative or infinite input becomes
/// zero and huge input is capped, so the conversion never panics.
pub(crate) fn safe_duration(secs: f32) -> std::time::Duration {
    if secs.is_finite() && secs > 0.0 {
        std::time::Duration::from_secs_f32(secs.min(MAX_EFFECT_SECS))
    } else {
        std::time::Duration::ZERO
    }
}

impl BusEffect {
    /// The time parameters (seconds) this effect carries, with a label for diagnostics.
    fn time_params(&self) -> Vec<(&'static str, f32)> {
        match self {
            Self::Bypassed(inner) => inner.time_params(),
            Self::Delay { delay_secs, .. } => vec![("delay_secs", *delay_secs)],
            Self::Convolution { predelay_secs, .. } => vec![("predelay_secs", *predelay_secs)],
            Self::Compressor {
                attack_secs,
                release_secs,
                ..
            } => vec![("attack_secs", *attack_secs), ("release_secs", *release_secs)],
            _ => Vec::new(),
        }
    }

    /// Whether this effect is bypassed ([`BusEffect::Bypassed`]).
    #[must_use]
    pub fn is_bypassed(&self) -> bool {
        matches!(self, Self::Bypassed(_))
    }

    /// The effect without its bypass wrapper (itself when it is not bypassed).
    #[must_use]
    pub fn active(&self) -> &BusEffect {
        match self {
            Self::Bypassed(inner) => inner.active(),
            other => other,
        }
    }

    /// This effect bypassed (`true`) or active (`false`); the parameters are kept either way and a
    /// bypassed effect is never wrapped twice.
    #[must_use]
    pub fn with_bypass(self, bypass: bool) -> Self {
        let inner = match self {
            Self::Bypassed(inner) => *inner,
            other => other,
        };
        if bypass {
            Self::Bypassed(Box::new(inner))
        } else {
            inner
        }
    }

    /// The stable kind name of the (unwrapped) effect, one of [`BUS_EFFECT_KINDS`].
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self.active() {
            Self::Eq { .. } => "eq",
            Self::LowPass { .. } => "low_pass",
            Self::HighPass { .. } => "high_pass",
            Self::Reverb { .. } => "reverb",
            Self::Delay { .. } => "delay",
            Self::Compressor { .. } => "compressor",
            Self::Distortion { .. } => "distortion",
            Self::Convolution { .. } => "convolution",
            // `active` never returns a wrapper.
            Self::Bypassed(_) => "bypassed",
        }
    }

    /// A new effect of `kind` ([`BUS_EFFECT_KINDS`]) at settings that leave the signal close to
    /// unchanged until edited (0 dB EQ, open filters, dry reverb and delay, 1:1 compressor, no
    /// drive, a dry convolution without an IR). `None` for an unknown kind.
    #[must_use]
    pub fn of_kind(kind: &str) -> Option<Self> {
        Some(match kind {
            "eq" => Self::Eq {
                low_hz: 200.0,
                low_db: 0.0,
                mid_hz: 1_000.0,
                mid_db: 0.0,
                high_hz: 5_000.0,
                high_db: 0.0,
                quality: 0.7,
            },
            "low_pass" => Self::neutral_low_pass(),
            "high_pass" => Self::HighPass {
                cutoff_hz: 10.0,
                resonance: 0.0,
            },
            "reverb" => Self::Reverb {
                feedback: 0.85,
                damping: 0.4,
                stereo_width: 1.0,
                mix: 0.0,
            },
            "delay" => Self::Delay {
                delay_secs: 0.25,
                feedback_db: -12.0,
                mix: 0.0,
            },
            "compressor" => Self::Compressor {
                threshold_db: 0.0,
                ratio: 1.0,
                attack_secs: 0.01,
                release_secs: 0.1,
                makeup_db: 0.0,
            },
            "distortion" => Self::Distortion { drive_db: 0.0 },
            "convolution" => Self::Convolution {
                ir: String::new(),
                mix: 0.0,
                predelay_secs: 0.0,
            },
            _ => return None,
        })
    }

    /// A bypass-equivalent low-pass (20 kHz, no resonance).
    pub fn neutral_low_pass() -> Self {
        Self::LowPass {
            cutoff_hz: 20_000.0,
            resonance: 0.0,
        }
    }
}

impl Default for BusEffect {
    fn default() -> Self {
        Self::neutral_low_pass()
    }
}

/// The send bus [`crate::AudioReverbZone`]s drive (the default tree's reverb).
pub const REVERB_BUS: &str = "Reverb";

/// The hidden send bus reverb zones with an impulse response play through (ADR-2721). It is not
/// part of any [`BusLayout`]: every bus that sends into [`REVERB_BUS`] also sends here, silently
/// until a zone with an IR holds the listener, so a project without zone IRs mixes exactly as
/// before.
pub const ZONE_IR_BUS: &str = "ZoneIr";

/// A send level that is silence to the mixer (kira treats −60 dB and below as silent).
pub const SILENT_SEND_DB: f32 = -60.0;

/// The volume a muted bus is set to, dB (ADR-544). kira treats −60 dB and below as silence; a
/// finite floor keeps every volume tween finite. Tweening to or from −∞ dB is not: the mute
/// lands in one step (a click) and the unmute interpolates `−∞ + ∞ = NaN`, which kira passes to
/// the output and into the reverb send's feedback, where it stays.
pub const MUTE_DB: f32 = -100.0;

/// Where a [`BusSend`] taps its bus (`audio-buses` v1, ADR-8590).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SendTap {
    /// After the bus's effects, fader and mute: the send follows the fader (the default, and
    /// the only tap before the field existed).
    #[default]
    PostFader,
    /// After the bus's effects, before its fader and mute: the send keeps its level when the
    /// fader is pulled down or the bus is muted (a reverb or monitor feed independent of the
    /// dry level).
    PreFader,
}

impl SendTap {
    /// True for [`SendTap::PostFader`] (the default, which the writer leaves out).
    #[must_use]
    pub fn is_post_fader(&self) -> bool {
        *self == Self::PostFader
    }
}

/// A send from one bus to a send bus, with the send volume in dB.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BusSend {
    /// Name of the send bus.
    pub bus: String,
    /// Volume of the signal sent, in dB.
    pub volume_db: f32,
    /// Where the send taps the bus (default [`SendTap::PostFader`]; a file without the field
    /// loads post-fader, and the writer leaves a post-fader tap out).
    #[serde(default, skip_serializing_if = "SendTap::is_post_fader")]
    pub tap: SendTap,
}

impl BusSend {
    /// A post-fader send into `bus` at `volume_db`.
    #[must_use]
    pub fn new(bus: impl Into<String>, volume_db: f32) -> Self {
        Self {
            bus: bus.into(),
            volume_db,
            tap: SendTap::PostFader,
        }
    }

    /// The same send with its tap set.
    #[must_use]
    pub fn with_tap(mut self, tap: SendTap) -> Self {
        self.tap = tap;
        self
    }
}

/// One bus definition: name, parent, volume, effects and sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BusDef {
    /// Bus name (`"Master"`, `"Sfx"`...). Unique across the layout.
    pub name: String,
    /// Parent bus, or `None` for the root (`Master`), which routes to the main track.
    #[serde(default)]
    pub parent: Option<String>,
    /// Initial volume in dB.
    #[serde(default)]
    pub volume_db: f32,
    /// Start muted.
    #[serde(default)]
    pub mute: bool,
    /// Effect chain, applied in order.
    #[serde(default)]
    pub effects: Vec<BusEffect>,
    /// Sends to send buses.
    #[serde(default)]
    pub sends: Vec<BusSend>,
    /// Voices on this bus (and its children) bypass the propagation medium: radio, comms, UI,
    /// music (`docs/specs/16-audio.md` § 3c). They are heard on an airless world, undelayed and
    /// unfiltered.
    #[serde(default)]
    pub bypass_medium: bool,
}

/// A send bus definition: effects shared by many buses (e.g. `Reverb`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SendBusDef {
    /// Send bus name.
    pub name: String,
    /// Effect chain of the send bus.
    #[serde(default)]
    pub effects: Vec<BusEffect>,
}

/// The whole bus tree. Parents must be defined before their children.
///
/// # Example
///
/// ```
/// use regia_audio::BusLayout;
///
/// let layout = BusLayout::default_tree();
/// layout.validate().expect("the default tree is valid");
/// assert_eq!(layout.buses[0].name, "Master");
/// assert!(layout.buses.iter().any(|b| b.name == "Sfx"));
/// assert!(layout.send_buses.iter().any(|s| s.name == "Reverb"));
/// ```
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BusLayout {
    /// The `audio-buses` format version (`docs/formats/audio-buses-ron.md`). A file without the
    /// field is version 0 (every `config/audio.ron` written before v0.6.0); the writer
    /// ([`BusLayout::to_ron`]) stamps [`AUDIO_BUSES_FORMAT_VERSION`].
    #[serde(default)]
    pub version: u32,
    /// All buses, parents first.
    pub buses: Vec<BusDef>,
    /// Send buses (no hierarchy, output to main).
    #[serde(default)]
    pub send_buses: Vec<SendBusDef>,
    /// Named mixer snapshots ([`AudioSnapshot`]), registered with the mixer when the layout is
    /// built (`audio-buses` v1). Names are unique and non-empty; every bus they name exists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snapshots: Vec<AudioSnapshot>,
}

/// The `audio-buses` format version this build reads and writes (`config/audio.ron`,
/// ADR-8105 (g)): 1 adds [`BusLayout::version`], [`BusLayout::snapshots`] and
/// [`BusEffect::Bypassed`]; 0 is the versionless shape.
pub const AUDIO_BUSES_FORMAT_VERSION: u32 = 1;

/// The versions [`BusLayout::parse`] migrates from (`cargo xtask check-formats`): 0, the
/// versionless shape, which only gains the `version` field.
#[must_use]
pub fn audio_buses_migration_sources() -> Vec<u32> {
    vec![0]
}

/// `config/audio.ron` text migrated to [`AUDIO_BUSES_FORMAT_VERSION`] with every other byte kept
/// (comments, order and formatting survive): a v0 file gains `version: 1` as the first field, a
/// v1 file is returned unchanged. The text must load ([`BusLayout::parse`]).
///
/// # Errors
///
/// The load error (a parse error, a newer version, a validation problem), or a top level that
/// is not a parenthesized struct.
pub fn migrate_audio_buses_ron(text: &str) -> Result<String, String> {
    BusLayout::parse(text)?;
    let stored = BusLayout::from_ron(text).map_err(|e| e.to_string())?.version;
    if stored == AUDIO_BUSES_FORMAT_VERSION {
        return Ok(text.to_owned());
    }
    let bytes = text.as_bytes();
    let mut at = 0;
    // Skip leading whitespace and comments, then an optional struct name.
    loop {
        match bytes.get(at).copied() {
            Some(b) if b.is_ascii_whitespace() => at += 1,
            Some(b'/') if bytes.get(at + 1) == Some(&b'/') => {
                while bytes.get(at).is_some_and(|b| *b != b'\n') {
                    at += 1;
                }
            }
            Some(b'/') if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                while at < bytes.len() && !(bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/')) {
                    at += 1;
                }
                at += 2;
            }
            _ => break,
        }
    }
    while bytes
        .get(at)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        at += 1;
    }
    if bytes.get(at) != Some(&b'(') {
        return Err("config/audio.ron: the top level is not a `( ... )` struct".into());
    }
    let open = at + 1;
    let insert = if bytes.get(open) == Some(&b'\n') || bytes.get(open..open + 2) == Some(&b"\r\n"[..]) {
        format!("\n    version: {AUDIO_BUSES_FORMAT_VERSION},")
    } else {
        format!("version: {AUDIO_BUSES_FORMAT_VERSION}, ")
    };
    Ok(format!("{}{insert}{}", &text[..open], &text[open..]))
}

/// The serde field-list fingerprint of `config/audio.ron` (`audio-buses`, hashed by
/// `cargo xtask check-formats`): every struct of the file and the effect variants with their fields.
#[must_use]
pub fn bus_layout_schema_fingerprint() -> String {
    use regia_core::schema::{
        join_field_lists, serde_field_names, serde_variant_names, serde_variant_shapes,
    };
    join_field_lists(&[
        serde_field_names::<BusLayout>(),
        serde_field_names::<BusDef>(),
        serde_field_names::<BusSend>(),
        serde_variant_names::<SendTap>(),
        serde_field_names::<SendBusDef>(),
        serde_field_names::<AudioSnapshot>(),
        serde_field_names::<crate::snapshot::SnapshotSend>(),
        serde_variant_names::<BusEffect>(),
        serde_variant_shapes::<BusEffect>(),
    ])
}

impl BusLayout {
    /// The spec's default tree (`docs/specs/16-audio.md` § 1):
    /// `Master → { Music, Sfx → { World, Ui }, Voice, Ambience }`, with a `Reverb` send bus fed by
    /// `World` and `Ambience`. Both sends start silent ([`SILENT_SEND_DB`]): the reverb is
    /// heard inside [`crate::AudioReverbZone`]s, which raise the sends and set the reverb's
    /// parameters while the listener is inside them (ADR-104).
    pub fn default_tree() -> Self {
        let reverb = BusEffect::Reverb {
            feedback: 0.85,
            damping: 0.4,
            stereo_width: 0.7,
            mix: 1.0,
        };
        Self {
            buses: vec![
                BusDef {
                    name: "Master".into(),
                    parent: None,
                    volume_db: 0.0,
                    mute: false,
                    effects: vec![],
                    sends: vec![],
                    bypass_medium: false,
                },
                BusDef {
                    name: "Music".into(),
                    parent: Some("Master".into()),
                    volume_db: -3.0,
                    mute: false,
                    effects: vec![],
                    sends: vec![],
                    bypass_medium: true,
                },
                BusDef {
                    name: "Sfx".into(),
                    parent: Some("Master".into()),
                    volume_db: 0.0,
                    mute: false,
                    effects: vec![],
                    sends: vec![],
                    bypass_medium: false,
                },
                BusDef {
                    name: "World".into(),
                    parent: Some("Sfx".into()),
                    volume_db: 0.0,
                    mute: false,
                    effects: vec![BusEffect::neutral_low_pass()],
                    sends: vec![BusSend::new(REVERB_BUS, SILENT_SEND_DB)],
                    bypass_medium: false,
                },
                BusDef {
                    name: "Ui".into(),
                    parent: Some("Sfx".into()),
                    volume_db: 0.0,
                    mute: false,
                    effects: vec![],
                    sends: vec![],
                    bypass_medium: true,
                },
                BusDef {
                    name: "Voice".into(),
                    parent: Some("Master".into()),
                    volume_db: 0.0,
                    mute: false,
                    effects: vec![],
                    sends: vec![],
                    bypass_medium: true,
                },
                BusDef {
                    name: "Ambience".into(),
                    parent: Some("Master".into()),
                    volume_db: -6.0,
                    mute: false,
                    effects: vec![],
                    sends: vec![BusSend::new(REVERB_BUS, SILENT_SEND_DB)],
                    bypass_medium: false,
                },
            ],
            send_buses: vec![SendBusDef {
                name: REVERB_BUS.into(),
                effects: vec![reverb],
            }],
            version: AUDIO_BUSES_FORMAT_VERSION,
            snapshots: Vec::new(),
        }
    }

    /// Whether voices on `bus` bypass the propagation medium: the bus or any ancestor sets
    /// [`BusDef::bypass_medium`]. Unknown buses do not bypass.
    pub fn bypasses_medium(&self, bus: &str) -> bool {
        let mut current = Some(bus);
        // The parent chain is acyclic in a validated layout; the bound guards a raw one.
        for _ in 0..=self.buses.len() {
            let Some(name) = current else {
                return false;
            };
            let Some(def) = self.buses.iter().find(|b| b.name == name) else {
                return false;
            };
            if def.bypass_medium {
                return true;
            }
            current = def.parent.as_deref();
        }
        false
    }

    /// Parses a layout from RON text (`config/audio.ron`) as it is stored: the version is kept
    /// (0 for a versionless file) and nothing is validated. [`BusLayout::parse`] is the loader.
    pub fn from_ron(text: &str) -> Result<Self, ron::error::SpannedError> {
        ron::from_str(text)
    }

    /// Loads `config/audio.ron` text: parses it, refuses a version newer than
    /// [`AUDIO_BUSES_FORMAT_VERSION`], migrates an older one (v0 → v1 only gains the version
    /// number) and validates it. The result is stamped at the current version.
    ///
    /// # Errors
    ///
    /// The parse error, the newer-version refusal (it names both versions) or the first
    /// validation problem.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut layout = Self::from_ron(text).map_err(|e| format!("config/audio.ron: {e}"))?;
        if layout.version > AUDIO_BUSES_FORMAT_VERSION {
            return Err(format!(
                "config/audio.ron is audio-buses version {}, newer than this build reads (version {AUDIO_BUSES_FORMAT_VERSION})",
                layout.version
            ));
        }
        layout.version = AUDIO_BUSES_FORMAT_VERSION;
        layout.validate()?;
        Ok(layout)
    }

    /// The canonical `config/audio.ron` text of this layout, stamped at
    /// [`AUDIO_BUSES_FORMAT_VERSION`].
    ///
    /// # Errors
    ///
    /// The serializer's error (a non-finite number cannot be written).
    pub fn to_ron(&self) -> Result<String, String> {
        let mut out = self.clone();
        out.version = AUDIO_BUSES_FORMAT_VERSION;
        ron::ser::to_string_pretty(&out, regia_core::ron_pretty()).map_err(|e| e.to_string())
    }

    /// The snapshot named `name`.
    #[must_use]
    pub fn snapshot(&self, name: &str) -> Option<&AudioSnapshot> {
        self.snapshots.iter().find(|s| s.name == name)
    }

    /// True when every parent reference resolves to a bus defined earlier in the list, every
    /// send has its send bus, every effect time is finite and non-negative, no effect is
    /// bypassed twice, the version is one this build reads, and every snapshot has a unique,
    /// non-empty name and names only buses (and sends) the layout defines.
    pub fn validate(&self) -> Result<(), String> {
        if self.version > AUDIO_BUSES_FORMAT_VERSION {
            return Err(format!(
                "audio-buses version {} is newer than this build reads (version {AUDIO_BUSES_FORMAT_VERSION})",
                self.version
            ));
        }
        self.validate_structure()?;
        self.validate_snapshots()?;
        let all_effects = self
            .buses
            .iter()
            .map(|b| (b.name.as_str(), &b.effects))
            .chain(self.send_buses.iter().map(|s| (s.name.as_str(), &s.effects)));
        for (owner, effects) in all_effects {
            for effect in effects {
                if let BusEffect::Bypassed(inner) = effect
                    && inner.is_bypassed()
                {
                    return Err(format!("bus `{owner}`: an effect is bypassed twice"));
                }
                for (what, secs) in effect.time_params() {
                    if !secs.is_finite() || secs < 0.0 {
                        return Err(format!(
                            "bus `{owner}`: {what} must be finite and non-negative, got {secs}"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// The snapshot part of [`Self::validate`].
    fn validate_snapshots(&self) -> Result<(), String> {
        let has_bus = |name: &str| self.buses.iter().any(|b| b.name == name);
        for (i, snapshot) in self.snapshots.iter().enumerate() {
            if snapshot.name.trim().is_empty() {
                return Err(format!("snapshot {i} has no name"));
            }
            if self.snapshots[..i].iter().any(|s| s.name == snapshot.name) {
                return Err(format!("snapshot `{}` is defined twice", snapshot.name));
            }
            let buses = snapshot
                .bus_volumes
                .keys()
                .chain(snapshot.filter_cutoffs.keys())
                .chain(snapshot.bus_mutes.keys());
            for bus in buses {
                if !has_bus(bus) {
                    return Err(format!(
                        "snapshot `{}` names bus `{bus}`, which the layout does not define",
                        snapshot.name
                    ));
                }
            }
            for (db, what) in snapshot
                .bus_volumes
                .values()
                .map(|v| (*v, "a bus volume"))
                .chain(
                    snapshot
                        .send_volumes
                        .iter()
                        .map(|s| (s.volume_db, "a send level")),
                )
                .chain(std::iter::once((snapshot.blend_secs, "its blend time")))
            {
                if !db.is_finite() {
                    return Err(format!("snapshot `{}`: {what} is not finite", snapshot.name));
                }
            }
            if snapshot.blend_secs < 0.0 {
                return Err(format!(
                    "snapshot `{}`: the blend time is negative",
                    snapshot.name
                ));
            }
            for send in &snapshot.send_volumes {
                let routed = self
                    .buses
                    .iter()
                    .any(|b| b.name == send.bus && b.sends.iter().any(|s| s.bus == send.send));
                if !routed {
                    return Err(format!(
                        "snapshot `{}` sets the send `{}` → `{}`, which the layout does not route",
                        snapshot.name, send.bus, send.send
                    ));
                }
            }
        }
        Ok(())
    }

    /// Whether `other` builds the same tracks as `self`: the same buses (names, parents,
    /// effects, send routes and their taps, medium bypass) and send buses. Only levels may differ (bus volumes,
    /// mutes, send levels, snapshots): such a change applies to a live mixer without a rebuild.
    #[must_use]
    pub fn same_tracks(&self, other: &BusLayout) -> bool {
        self.send_buses == other.send_buses
            && self.buses.len() == other.buses.len()
            && self.buses.iter().zip(&other.buses).all(|(a, b)| {
                a.name == b.name
                    && a.parent == b.parent
                    && a.effects == b.effects
                    && a.bypass_medium == b.bypass_medium
                    && a.sends.len() == b.sends.len()
                    && a.sends
                        .iter()
                        .zip(&b.sends)
                        .all(|(x, y)| x.bus == y.bus && x.tap == y.tap)
            })
    }

    /// The structural part of [`Self::validate`] (names, parents, sends): what
    /// [`Buses::build`] needs. Effect times are not checked here; the build neutralises a bad
    /// one (1 ms delay floor, zero attack/release) instead of dropping the whole bus tree.
    fn validate_structure(&self) -> Result<(), String> {
        let mut seen: Vec<&str> = Vec::new();
        for bus in &self.buses {
            if seen.contains(&bus.name.as_str()) {
                return Err(format!("bus `{}` is defined twice", bus.name));
            }
            if let Some(parent) = &bus.parent {
                if bus.name == *parent {
                    return Err(format!("bus `{}` is its own parent", bus.name));
                }
                if !seen.contains(&parent.as_str()) {
                    return Err(format!(
                        "bus `{}` references parent `{}` which is not defined before it",
                        bus.name, parent
                    ));
                }
            }
            seen.push(&bus.name);
        }
        for send in &bus_sends(self) {
            if !self.send_buses.iter().any(|s| s.name == *send) {
                return Err(format!("send `{send}` has no send bus"));
            }
        }
        Ok(())
    }
}

/// All send targets referenced by the buses (for validation).
fn bus_sends(layout: &BusLayout) -> Vec<&str> {
    layout
        .buses
        .iter()
        .flat_map(|b| b.sends.iter().map(|s| s.bus.as_str()))
        .collect()
}

/// A live filter handle on a bus, for runtime cutoff tweaks.
pub enum BusFilterHandle {
    /// A low/high-pass filter.
    Filter(FilterHandle),
}

/// The live buses built from a [`BusLayout`]: kira tracks plus bookkeeping.
pub struct Buses {
    layout: BusLayout,
    /// The fader track of every bus: its volume is the bus fader and mute, and it carries the
    /// post-fader sends.
    tracks: HashMap<String, TrackHandle>,
    /// The input track of every bus with a pre-fader send (ADR-8590): a sub-track of the fader
    /// track at unity that sounds and child buses play into, with the bus's effects, meter and
    /// pre-fader sends. kira sends after a track's volume, so the pre-fader sends live on a track
    /// whose own volume never moves, and the fader is the parent's volume.
    inputs: HashMap<String, TrackHandle>,
    filters: HashMap<String, Vec<BusFilterHandle>>,
    base_volume_db: HashMap<String, f32>,
    /// Send buses. Held for the buses' lifetime: dropping a kira send-track handle removes the
    /// track from the mixer (and with it every send into it).
    send_tracks: HashMap<String, SendTrackHandle>,
    /// Live reverb effects per send bus (reverb zones retarget them).
    send_reverbs: HashMap<String, Vec<ReverbHandle>>,
    /// Live convolution effects: `(bus, index among the bus's convolutions, handle)`.
    convolutions: Vec<(String, usize, crate::ConvolutionHandle)>,
    /// The zone-IR send bus's track and convolver (`None` when the layout has no reverb bus).
    zone_ir: Option<(SendTrackHandle, crate::ConvolutionHandle)>,
    /// The silent send tracks a render-out mixdown taps regular buses through (ADR-1520). Only
    /// held, never read: dropping a handle would remove its track. Empty outside a mixdown.
    _tap_tracks: Vec<SendTrackHandle>,
    /// Level meters, one per regular bus (`bus_meter.rs`, ADR-2530).
    meters: HashMap<String, crate::bus_meter::BusMeterHandle>,
}

/// Creates the kira tracks a bus layout needs. Implemented by the output backends (both kira
/// manager flavours) so [`Buses::build`] needs a single mutable borrow.
pub trait TrackHost {
    /// Adds a sub-track routed to the main track (the root bus).
    fn add_sub_track(&mut self, builder: TrackBuilder) -> Result<TrackHandle, kira::ResourceLimitReached>;
    /// Adds a send bus.
    fn add_send_track(
        &mut self,
        builder: SendTrackBuilder,
    ) -> Result<SendTrackHandle, kira::ResourceLimitReached>;
}

/// A [`TrackHost`] that never creates anything (building an empty layout).
pub struct NoHost;

impl TrackHost for NoHost {
    fn add_sub_track(&mut self, _builder: TrackBuilder) -> Result<TrackHandle, kira::ResourceLimitReached> {
        Err(kira::ResourceLimitReached)
    }

    fn add_send_track(
        &mut self,
        _builder: SendTrackBuilder,
    ) -> Result<SendTrackHandle, kira::ResourceLimitReached> {
        Err(kira::ResourceLimitReached)
    }
}

impl Buses {
    /// Builds all tracks of `layout` on `host`. The root bus and send buses are created through
    /// the host; child buses are created through their parent's handle so the kira hierarchy
    /// mirrors the layout.
    pub fn build(layout: &BusLayout, host: &mut dyn TrackHost) -> Result<Self, String> {
        Self::build_tapped(layout, host, &[], &ConvolutionContext::default())
    }

    /// [`Self::build`] with stem taps (a render-out mixdown, ADR-1520). A tapped regular bus gets
    /// a 0 dB send into a send track of its own whose last effect is the tap and whose volume is
    /// silence, so the tap hears the bus after its fader, mute and ducking (kira applies a track's
    /// volume after its effects and sends after its volume) and nothing reaches the mix. A tapped
    /// send bus gets the tap at the end of its own effect chain (send buses have no fader).
    ///
    /// A bus with a [`SendTap::PreFader`] send is built as two tracks (ADR-8590): a fader track
    /// (the bus volume and mute, the post-fader sends, the stem tap) and under it an input track
    /// at unity (voices and child buses, the effects, the meter, the pre-fader sends). Every
    /// other bus is one track, as before.
    pub(crate) fn build_tapped(
        layout: &BusLayout,
        host: &mut dyn TrackHost,
        taps: &[(String, crate::audio::StemTap)],
        conv: &ConvolutionContext,
    ) -> Result<Self, String> {
        // Structure only: a NaN or negative effect time is neutralised below, so one bad effect
        // does not cost the game every bus (`BusLayout::validate` still reports it).
        layout.validate_structure()?;
        let mut send_tracks = HashMap::new();
        let mut send_reverbs: HashMap<String, Vec<ReverbHandle>> = HashMap::new();
        let mut convolutions = Vec::new();
        for send in &layout.send_buses {
            let mut builder = SendTrackBuilder::new();
            let mut reverbs = Vec::new();
            let mut index = 0;
            for effect in &send.effects {
                if let BusEffect::Convolution { .. } = effect {
                    let handle = conv.add_effect(effect, true, |b| {
                        builder.add_effect(b);
                    });
                    convolutions.push((send.name.clone(), index, handle));
                    index += 1;
                } else if let Some(reverb) = apply_send_effect(&mut builder, effect) {
                    reverbs.push(reverb);
                }
            }
            for (_, tap) in taps.iter().filter(|(bus, _)| *bus == send.name) {
                builder.add_effect(tap.clone());
            }
            let handle = host
                .add_send_track(builder)
                .map_err(|e| format!("send track: {e:?}"))?;
            send_tracks.insert(send.name.clone(), handle);
            send_reverbs.insert(send.name.clone(), reverbs);
        }

        // The zone-IR bus: wet only, no IR until a zone names one (ADR-2721).
        let zone_ir = if layout.send_buses.iter().any(|s| s.name == REVERB_BUS) {
            let (effect, handle) = crate::ConvolutionEffectBuilder::new(conv.sample_rate, None, 1.0, None);
            let builder = SendTrackBuilder::new().with_effect(effect);
            let track = host
                .add_send_track(builder)
                .map_err(|e| format!("zone IR send track: {e:?}"))?;
            Some((track, handle))
        } else {
            None
        };

        // One silent tap track per tapped regular bus, made before the buses that send into it.
        let mut tap_tracks = Vec::new();
        let mut tap_of: HashMap<&str, usize> = HashMap::new();
        for (bus, tap) in taps {
            if !layout.buses.iter().any(|b| b.name == *bus) || tap_of.contains_key(bus.as_str()) {
                continue;
            }
            let builder = SendTrackBuilder::new()
                .volume(Decibels::SILENCE)
                .with_effect(tap.clone());
            let handle = host
                .add_send_track(builder)
                .map_err(|e| format!("stem tap of {bus}: {e:?}"))?;
            tap_of.insert(bus.as_str(), tap_tracks.len());
            tap_tracks.push(handle);
        }

        let mut tracks: HashMap<String, TrackHandle> = HashMap::new();
        let mut filters: HashMap<String, Vec<BusFilterHandle>> = HashMap::new();
        let mut base_volume_db: HashMap<String, f32> = HashMap::new();
        let mut meters: HashMap<String, crate::bus_meter::BusMeterHandle> = HashMap::new();
        let mut inputs: HashMap<String, TrackHandle> = HashMap::new();
        for bus in &layout.buses {
            // A layout-muted bus is built silent, so it is silent from its first rendered sample.
            let initial_db = if bus.mute { MUTE_DB } else { bus.volume_db };
            // A bus with a pre-fader send is split (ADR-8590): `builder` becomes the input track
            // (unity volume, effects, meter, pre-fader sends) under a fader track that carries
            // the volume and the post-fader sends. Otherwise one track does both.
            let split = bus
                .sends
                .iter()
                .any(|s| s.tap == SendTap::PreFader && send_tracks.contains_key(&s.bus));
            let mut fader = TrackBuilder::new()
                .volume(Decibels(initial_db))
                .sound_capacity(1)
                .sub_track_capacity(1);
            let mut builder = TrackBuilder::new()
                .volume(if split {
                    Decibels::IDENTITY
                } else {
                    Decibels(initial_db)
                })
                .sound_capacity(512)
                .sub_track_capacity(512);
            let mut bus_filters = Vec::new();
            let mut index = 0;
            for effect in &bus.effects {
                if let BusEffect::Convolution { .. } = effect {
                    let handle = conv.add_effect(effect, false, |b| {
                        builder.add_effect(b);
                    });
                    convolutions.push((bus.name.clone(), index, handle));
                    index += 1;
                } else if let Some(handle) = apply_bus_effect(&mut builder, effect) {
                    bus_filters.push(BusFilterHandle::Filter(handle));
                }
            }
            // The meter is the last effect: it hears the bus after its own effects.
            let (meter_builder, meter) = crate::bus_meter::BusMeterBuilder::new();
            meter.set_fader_db(initial_db);
            builder.add_effect(meter_builder);
            meters.insert(bus.name.clone(), meter);
            for send in &bus.sends {
                if let Some(target) = send_tracks.get(&send.bus) {
                    let level = Decibels(send.volume_db);
                    if split && send.tap == SendTap::PostFader {
                        fader = fader.with_send(target, level);
                    } else {
                        builder = builder.with_send(target, level);
                    }
                }
            }
            // The zone-IR send follows the reverb send's tap.
            if let Some((zone_track, _)) = &zone_ir
                && let Some(reverb) = bus.sends.iter().find(|send| send.bus == REVERB_BUS)
            {
                if split && reverb.tap == SendTap::PostFader {
                    fader = fader.with_send(zone_track, Decibels(SILENT_SEND_DB));
                } else {
                    builder = builder.with_send(zone_track, Decibels(SILENT_SEND_DB));
                }
            }
            // A stem tap hears the bus after its fader: on the fader track of a split bus.
            if let Some(tap) = tap_of.get(bus.name.as_str()).and_then(|i| tap_tracks.get(*i)) {
                if split {
                    fader = fader.with_send(tap, Decibels::IDENTITY);
                } else {
                    builder = builder.with_send(tap, Decibels::IDENTITY);
                }
            }
            let (top, input) = if split {
                (fader, Some(builder))
            } else {
                (builder, None)
            };
            let mut handle = match &bus.parent {
                None => host
                    .add_sub_track(top)
                    .map_err(|e| format!("bus {}: {e:?}", bus.name))?,
                Some(parent) => {
                    // Children mix into the parent's input track, ahead of its effects.
                    let parent_track = match inputs.get_mut(parent) {
                        Some(input) => input,
                        None => tracks.get_mut(parent).ok_or_else(|| {
                            format!("bus `{}` references missing parent `{}`", bus.name, parent)
                        })?,
                    };
                    parent_track
                        .add_sub_track(top)
                        .map_err(|e| format!("bus {}: {e:?}", bus.name))?
                }
            };
            if let Some(input) = input {
                let input = handle
                    .add_sub_track(input)
                    .map_err(|e| format!("bus {} (pre-fader input): {e:?}", bus.name))?;
                inputs.insert(bus.name.clone(), input);
            }
            if bus.mute {
                handle.set_volume(Decibels(MUTE_DB), tween(std::time::Duration::ZERO));
            }
            filters.insert(bus.name.clone(), bus_filters);
            base_volume_db.insert(bus.name.clone(), bus.volume_db);
            tracks.insert(bus.name.clone(), handle);
        }

        Ok(Self {
            layout: layout.clone(),
            tracks,
            inputs,
            filters,
            base_volume_db,
            send_tracks,
            send_reverbs,
            convolutions,
            zone_ir,
            _tap_tracks: tap_tracks,
            meters,
        })
    }

    /// Retargets the level of `bus`'s send into the hidden [`ZONE_IR_BUS`]. `None` when the bus
    /// does not feed the reverb or the layout has no reverb bus.
    pub fn set_zone_ir_send(&mut self, bus: &str, db: f32, fade: std::time::Duration) -> Option<()> {
        let id = self.zone_ir.as_ref()?.0.id();
        // The zone-IR send follows the bus's reverb send's tap.
        let track = self.send_carrier(bus, REVERB_BUS)?;
        track.set_send(id, Decibels(finite_db(db)), tween(fade)).ok()
    }

    /// The track that carries `bus`'s send into `send_bus`: the input track for a pre-fader
    /// send of a split bus, else the bus's fader track.
    fn send_carrier(&mut self, bus: &str, send_bus: &str) -> Option<&mut TrackHandle> {
        let pre = self
            .layout
            .buses
            .iter()
            .find(|b| b.name == bus)
            .and_then(|b| b.sends.iter().find(|s| s.bus == send_bus))
            .is_some_and(|s| s.tap == SendTap::PreFader);
        match self.inputs.get_mut(bus) {
            Some(input) if pre => Some(input),
            _ => self.tracks.get_mut(bus),
        }
    }

    /// The zone-IR convolver (`None` when the layout has no reverb bus).
    pub fn zone_convolver(&mut self) -> Option<&mut crate::ConvolutionHandle> {
        self.zone_ir.as_mut().map(|(_, handle)| handle)
    }

    /// Every bus convolution: `(bus, index among that bus's convolutions, handle)`.
    pub fn convolutions(&self) -> impl Iterator<Item = (&str, usize, &crate::ConvolutionHandle)> {
        self.convolutions.iter().map(|(bus, i, h)| (bus.as_str(), *i, h))
    }

    /// The convolution `index` (0 = first) on `bus`.
    pub fn convolution_mut(&mut self, bus: &str, index: usize) -> Option<&mut crate::ConvolutionHandle> {
        self.convolutions
            .iter_mut()
            .find(|(b, i, _)| b == bus && *i == index)
            .map(|(_, _, h)| h)
    }

    /// Frees convolution slots the audio thread retired (once a frame).
    pub fn collect_convolution_garbage(&mut self) {
        for (_, _, handle) in &self.convolutions {
            handle.collect();
        }
        if let Some((_, handle)) = &self.zone_ir {
            handle.collect();
        }
    }

    /// Every `(bus, layout send level in dB)` that sends into `send_bus`.
    pub fn sends_into(&self, send_bus: &str) -> Vec<(String, f32)> {
        self.layout
            .buses
            .iter()
            .flat_map(|bus| {
                bus.sends
                    .iter()
                    .filter(|send| send.bus == send_bus)
                    .map(|send| (bus.name.clone(), send.volume_db))
            })
            .collect()
    }

    /// Retargets the level of `bus`'s send into `send_bus`. `None` when either is unknown or
    /// the layout has no such route.
    pub fn set_send_volume(
        &mut self,
        bus: &str,
        send_bus: &str,
        db: f32,
        fade: std::time::Duration,
    ) -> Option<()> {
        let id = self.send_tracks.get(send_bus)?.id();
        let track = self.send_carrier(bus, send_bus)?;
        track.set_send(id, Decibels(finite_db(db)), tween(fade)).ok()
    }

    /// Retargets every reverb on a send bus. Returns how many reverbs were found.
    pub fn set_send_reverb(
        &mut self,
        send_bus: &str,
        feedback: f64,
        damping: f64,
        stereo_width: f64,
        fade: std::time::Duration,
    ) -> usize {
        let Some(list) = self.send_reverbs.get_mut(send_bus) else {
            return 0;
        };
        for reverb in list.iter_mut() {
            reverb.set_feedback(feedback, tween(fade));
            reverb.set_damping(damping, tween(fade));
            reverb.set_stereo_width(stereo_width, tween(fade));
        }
        list.len()
    }

    /// Adds a child track under `bus` (per-emitter processing: occlusion/absorption low-pass,
    /// binaural rendering). The child mixes into the bus, so bus volume, ducking, snapshots and
    /// sends apply to it. `None` for an unknown bus or when the mixer refuses the track.
    pub fn add_child_track(&mut self, bus: &str, builder: TrackBuilder) -> Option<TrackHandle> {
        let track = self.input_track(bus)?;
        match track.add_sub_track(builder) {
            Ok(handle) => Some(handle),
            Err(error) => {
                tracing::warn!(target: "regia_audio", "child track under `{bus}` rejected: {error:?}");
                None
            }
        }
    }

    /// Takes the levels of `layout` (bus volumes, mutes, send levels, snapshots), which builds
    /// the same tracks ([`BusLayout::same_tracks`]): what [`Self::sends_into`],
    /// [`Self::base_volume_db`] and [`Self::base_muted`] answer from then on.
    pub(crate) fn set_layout_levels(&mut self, layout: &BusLayout) {
        for bus in &layout.buses {
            self.base_volume_db.insert(bus.name.clone(), bus.volume_db);
        }
        self.layout = layout.clone();
    }

    /// The layout the buses were built from.
    pub fn layout(&self) -> &BusLayout {
        &self.layout
    }

    /// Handle of a bus, for playing sounds on it: the track its voices and child buses mix
    /// into, before its effects, pre-fader sends and fader. Set the bus volume with
    /// [`Self::set_volume`], not on this handle (on a bus with a pre-fader send it is the input
    /// track, whose own volume stays at unity).
    pub fn track(&mut self, name: &str) -> Option<&mut TrackHandle> {
        self.input_track(name)
    }

    /// The input track of `name`: the pre-fader input of a split bus, else its only track.
    fn input_track(&mut self, name: &str) -> Option<&mut TrackHandle> {
        match self.inputs.get_mut(name) {
            Some(input) => Some(input),
            None => self.tracks.get_mut(name),
        }
    }

    /// Whether `name` is split into a fader track and a pre-fader input track (it has a
    /// pre-fader send, ADR-8590).
    #[must_use]
    pub fn has_prefader_input(&self, name: &str) -> bool {
        self.inputs.contains_key(name)
    }

    /// True when the layout defines `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.tracks.contains_key(name)
    }

    /// Names of all buses.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tracks.keys().map(String::as_str)
    }

    /// The level meter of a bus (`None` for an unknown bus or send bus).
    pub fn meter(&self, name: &str) -> Option<&crate::bus_meter::BusMeterHandle> {
        self.meters.get(name)
    }

    /// The layout's initial volume of a bus in dB.
    pub fn base_volume_db(&self, name: &str) -> Option<f32> {
        self.base_volume_db.get(name).copied()
    }

    /// Whether the layout starts the bus muted.
    pub fn base_muted(&self, name: &str) -> bool {
        self.layout.buses.iter().any(|b| b.name == name && b.mute)
    }

    /// Sets the post-effects volume of a bus, tweened over `fade`. The level is kept within
    /// [`MUTE_DB`]`..=`[`MAX_BUS_DB`] (NaN is muted), so no tween can reach −∞ or NaN.
    pub fn set_volume(&mut self, name: &str, db: f32, fade: std::time::Duration) -> Option<()> {
        let track = self.tracks.get_mut(name)?;
        let db = finite_db(db);
        track.set_volume(Decibels(db), tween(fade));
        if let Some(meter) = self.meters.get(name) {
            meter.set_fader_db(db);
        }
        Some(())
    }

    /// Mutes (`true`) or unmutes a bus, fading over `fade`. Mute is [`MUTE_DB`] (silence to the
    /// mixer); unmute returns to the layout volume.
    pub fn set_mute(&mut self, name: &str, mute: bool, fade: std::time::Duration) -> Option<()> {
        let track = self.tracks.get_mut(name)?;
        let db = if mute {
            MUTE_DB
        } else {
            self.base_volume_db.get(name).copied().unwrap_or(0.0)
        };
        track.set_volume(Decibels(db), tween(fade));
        if let Some(meter) = self.meters.get(name) {
            meter.set_fader_db(db);
        }
        Some(())
    }

    /// Retargets the cutoff of every low/high-pass filter on a bus. Returns how many filters were
    /// found (0 when the bus has none, or when `cutoff_hz` is not finite: such a cutoff is ignored).
    pub fn set_filter_cutoffs(&mut self, name: &str, cutoff_hz: f64, fade: std::time::Duration) -> usize {
        // A NaN cutoff turns the mixer filter's state NaN for good (the bus stays silent after the
        // snapshot is popped).
        // Regression: tests/tqs_sweep.rs::a_nan_snapshot_cutoff_does_not_kill_the_bus_regression
        if !cutoff_hz.is_finite() {
            return 0;
        }
        let Some(list) = self.filters.get_mut(name) else {
            return 0;
        };
        let mut found = 0;
        for BusFilterHandle::Filter(filter) in list {
            filter.set_cutoff(cutoff_hz, tween(fade));
            found += 1;
        }
        found
    }
}

impl Buses {
    /// Retargets every low/high-pass filter on a bus back to the cutoff the layout authored for
    /// it (undoing a snapshot's override). Returns how many filters were found.
    pub fn restore_filter_cutoffs(&mut self, name: &str, fade: std::time::Duration) -> usize {
        let Some(def) = self.layout.buses.iter().find(|b| b.name == name) else {
            return 0;
        };
        let authored = def.effects.iter().filter_map(|effect| match effect {
            BusEffect::LowPass { cutoff_hz, .. } | BusEffect::HighPass { cutoff_hz, .. } => {
                Some(f64::from(*cutoff_hz))
            }
            _ => None,
        });
        let Some(list) = self.filters.get_mut(name) else {
            return 0;
        };
        let mut found = 0;
        for (BusFilterHandle::Filter(filter), cutoff) in list.iter_mut().zip(authored) {
            filter.set_cutoff(cutoff, tween(fade));
            found += 1;
        }
        found
    }
}

/// The loudest bus or send level accepted, dB.
pub const MAX_BUS_DB: f32 = 48.0;

/// `db` within [`MUTE_DB`]`..=`[`MAX_BUS_DB`]; NaN becomes [`MUTE_DB`].
pub(crate) fn finite_db(db: f32) -> f32 {
    if db.is_nan() {
        MUTE_DB
    } else {
        db.clamp(MUTE_DB, MAX_BUS_DB)
    }
}

/// Builds a tween with a linear ramp (our envelopes are computed on the game thread, so kira only
/// needs to interpolate the short hop between two consecutive targets).
pub(crate) fn tween(fade: std::time::Duration) -> Tween {
    Tween {
        duration: fade,
        easing: kira::Easing::Linear,
        ..Tween::default()
    }
}

/// Longest delay a bus effect gets, seconds: kira sizes the delay line from it when the bus is
/// built, so an authored hour-long delay would allocate gigabytes.
const MAX_DELAY_SECS: f32 = 10.0;

/// Shortest delay a bus effect gets: kira processes its delay line in chunks of the line's length,
/// and a zero-frame line (a zero, negative or NaN delay) panics on the mixer thread. 1 ms is at
/// least one frame at any rate kira runs.
const MIN_DELAY: std::time::Duration = std::time::Duration::from_millis(1);

/// `effect` with every NaN or infinite parameter replaced by a neutral value (counted as a
/// refusal). kira's filters, EQ, compressor and reverb keep a NaN in their state for the life of
/// the bus, so one bad number in an authored layout would silence the bus and everything it feeds.
/// Times are handled where they become durations. `send` picks the neutral reverb mix: fully wet on
/// a send bus (its input is the send), dry on an ordinary bus (its own signal must pass).
fn finite_effect(effect: &BusEffect, send: bool) -> BusEffect {
    let fix = |value: f32, neutral: f32| {
        if value.is_finite() {
            value
        } else {
            crate::sound::note_rejected("bus effect value");
            neutral
        }
    };
    if let BusEffect::Convolution {
        ir,
        mix,
        predelay_secs,
    } = effect
    {
        return BusEffect::Convolution {
            ir: ir.clone(),
            mix: fix(*mix, if send { 1.0 } else { 0.0 }),
            predelay_secs: fix(*predelay_secs, 0.0),
        };
    }
    match *effect {
        BusEffect::Eq {
            low_hz,
            low_db,
            mid_hz,
            mid_db,
            high_hz,
            high_db,
            quality,
        } => BusEffect::Eq {
            low_hz: fix(low_hz, 200.0),
            low_db: fix(low_db, 0.0),
            mid_hz: fix(mid_hz, 1_000.0),
            mid_db: fix(mid_db, 0.0),
            high_hz: fix(high_hz, 5_000.0),
            high_db: fix(high_db, 0.0),
            quality: fix(quality, 0.7),
        },
        BusEffect::LowPass { cutoff_hz, resonance } => BusEffect::LowPass {
            cutoff_hz: fix(cutoff_hz, 20_000.0),
            resonance: fix(resonance, 0.0),
        },
        BusEffect::HighPass { cutoff_hz, resonance } => BusEffect::HighPass {
            cutoff_hz: fix(cutoff_hz, 10.0),
            resonance: fix(resonance, 0.0),
        },
        BusEffect::Reverb {
            feedback,
            damping,
            stereo_width,
            mix,
        } => BusEffect::Reverb {
            feedback: fix(feedback, 0.85),
            damping: fix(damping, 0.4),
            stereo_width: fix(stereo_width, 1.0),
            mix: fix(mix, if send { 1.0 } else { 0.0 }),
        },
        BusEffect::Delay {
            delay_secs,
            feedback_db,
            mix,
        } => BusEffect::Delay {
            delay_secs,
            feedback_db: fix(feedback_db, SILENT_SEND_DB),
            mix: fix(mix, 0.0),
        },
        BusEffect::Compressor {
            threshold_db,
            ratio,
            attack_secs,
            release_secs,
            makeup_db,
        } => BusEffect::Compressor {
            threshold_db: fix(threshold_db, 0.0),
            ratio: fix(ratio, 1.0),
            attack_secs,
            release_secs,
            makeup_db: fix(makeup_db, 0.0),
        },
        BusEffect::Distortion { drive_db } => BusEffect::Distortion {
            drive_db: fix(drive_db, 0.0),
        },
        // Handled above; a bypassed effect is not built.
        BusEffect::Convolution { .. } | BusEffect::Bypassed(_) => effect.clone(),
    }
}

/// Adds one effect to a send-track builder; returns the live handle when it is a reverb (reverb
/// zones retarget it).
fn apply_send_effect(builder: &mut SendTrackBuilder, effect: &BusEffect) -> Option<ReverbHandle> {
    let effect = &finite_effect(effect, true);
    match effect {
        BusEffect::Eq {
            low_hz,
            low_db,
            mid_hz,
            mid_db,
            high_hz,
            high_db,
            quality,
        } => {
            builder.add_effect(EqFilterBuilder::new(
                EqFilterKind::LowShelf,
                *low_hz as f64,
                Decibels(*low_db),
                *quality as f64,
            ));
            builder.add_effect(EqFilterBuilder::new(
                EqFilterKind::Bell,
                *mid_hz as f64,
                Decibels(*mid_db),
                *quality as f64,
            ));
            builder.add_effect(EqFilterBuilder::new(
                EqFilterKind::HighShelf,
                *high_hz as f64,
                Decibels(*high_db),
                *quality as f64,
            ));
        }
        BusEffect::LowPass { cutoff_hz, resonance } => {
            builder.add_effect(
                FilterBuilder::new()
                    .mode(FilterMode::LowPass)
                    .cutoff(*cutoff_hz as f64)
                    .resonance(*resonance as f64),
            );
        }
        BusEffect::HighPass { cutoff_hz, resonance } => {
            builder.add_effect(
                FilterBuilder::new()
                    .mode(FilterMode::HighPass)
                    .cutoff(*cutoff_hz as f64)
                    .resonance(*resonance as f64),
            );
        }
        BusEffect::Delay {
            delay_secs,
            feedback_db,
            mix,
        } => {
            builder.add_effect(
                DelayBuilder::new()
                    .delay_time(crate::sound::duration_from_secs(*delay_secs, MAX_DELAY_SECS).max(MIN_DELAY))
                    .feedback(Decibels(*feedback_db))
                    .mix(*mix),
            );
        }
        BusEffect::Compressor {
            threshold_db,
            ratio,
            attack_secs,
            release_secs,
            makeup_db,
        } => {
            builder.add_effect(
                CompressorBuilder::new()
                    .threshold(*threshold_db as f64)
                    .ratio(*ratio as f64)
                    .attack_duration(safe_duration(*attack_secs))
                    .release_duration(safe_duration(*release_secs))
                    .makeup_gain(Decibels(*makeup_db)),
            );
        }
        BusEffect::Distortion { drive_db } => {
            builder.add_effect(
                DistortionBuilder::new()
                    .kind(DistortionKind::SoftClip)
                    .drive(Decibels(*drive_db)),
            );
        }
        // Built by `ConvolutionContext::add_effect` (the caller routes it there); a bypassed
        // effect is not built, so its slot passes the signal unchanged.
        BusEffect::Convolution { .. } | BusEffect::Bypassed(_) => {}
        BusEffect::Reverb {
            feedback,
            damping,
            stereo_width,
            mix,
        } => {
            return Some(
                builder.add_effect(
                    ReverbBuilder::new()
                        .feedback(*feedback as f64)
                        .damping(*damping as f64)
                        .stereo_width(*stereo_width as f64)
                        .mix(*mix),
                ),
            );
        }
    }
    None
}

/// Adds one effect to a sub-track builder; returns the live handle when the effect is a filter
/// (the only kind whose parameters we retarget at runtime in this slice).
fn apply_bus_effect(builder: &mut TrackBuilder, effect: &BusEffect) -> Option<FilterHandle> {
    let effect = &finite_effect(effect, false);
    match effect {
        BusEffect::Eq {
            low_hz,
            low_db,
            mid_hz,
            mid_db,
            high_hz,
            high_db,
            quality,
        } => {
            builder.add_effect(EqFilterBuilder::new(
                EqFilterKind::LowShelf,
                *low_hz as f64,
                Decibels(*low_db),
                *quality as f64,
            ));
            builder.add_effect(EqFilterBuilder::new(
                EqFilterKind::Bell,
                *mid_hz as f64,
                Decibels(*mid_db),
                *quality as f64,
            ));
            builder.add_effect(EqFilterBuilder::new(
                EqFilterKind::HighShelf,
                *high_hz as f64,
                Decibels(*high_db),
                *quality as f64,
            ));
            None
        }
        BusEffect::LowPass { cutoff_hz, resonance } => Some(
            builder.add_effect(
                FilterBuilder::new()
                    .mode(FilterMode::LowPass)
                    .cutoff(*cutoff_hz as f64)
                    .resonance(*resonance as f64),
            ),
        ),
        BusEffect::HighPass { cutoff_hz, resonance } => Some(
            builder.add_effect(
                FilterBuilder::new()
                    .mode(FilterMode::HighPass)
                    .cutoff(*cutoff_hz as f64)
                    .resonance(*resonance as f64),
            ),
        ),
        BusEffect::Reverb {
            feedback,
            damping,
            stereo_width,
            mix,
        } => {
            builder.add_effect(
                ReverbBuilder::new()
                    .feedback(*feedback as f64)
                    .damping(*damping as f64)
                    .stereo_width(*stereo_width as f64)
                    .mix(*mix),
            );
            None
        }
        BusEffect::Delay {
            delay_secs,
            feedback_db,
            mix,
        } => {
            builder.add_effect(
                DelayBuilder::new()
                    .delay_time(crate::sound::duration_from_secs(*delay_secs, MAX_DELAY_SECS).max(MIN_DELAY))
                    .feedback(Decibels(*feedback_db))
                    .mix(*mix),
            );
            None
        }
        BusEffect::Compressor {
            threshold_db,
            ratio,
            attack_secs,
            release_secs,
            makeup_db,
        } => {
            builder.add_effect(
                CompressorBuilder::new()
                    .threshold(*threshold_db as f64)
                    .ratio(*ratio as f64)
                    .attack_duration(safe_duration(*attack_secs))
                    .release_duration(safe_duration(*release_secs))
                    .makeup_gain(Decibels(*makeup_db)),
            );
            None
        }
        BusEffect::Distortion { drive_db } => {
            builder.add_effect(
                DistortionBuilder::new()
                    .kind(DistortionKind::SoftClip)
                    .drive(Decibels(*drive_db)),
            );
            None
        }
        // Built by `ConvolutionContext::add_effect` (the caller routes it there); a bypassed
        // effect is not built, so its slot passes the signal unchanged.
        BusEffect::Convolution { .. } | BusEffect::Bypassed(_) => None,
    }
}

/// What a bus build needs to make convolution effects: the prepared kernels by IR name at the
/// build's sample rate, and the tier (ADR-2721).
#[derive(Clone, Debug)]
pub(crate) struct ConvolutionContext {
    /// The rate the buses render at.
    pub sample_rate: u32,
    /// The device tier.
    pub tier: crate::ConvolutionTier,
    /// Prepared kernels by IR name.
    pub kernels: HashMap<String, std::sync::Arc<crate::ConvolutionKernel>>,
}

impl Default for ConvolutionContext {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            tier: crate::ConvolutionTier::platform_default(),
            kernels: HashMap::new(),
        }
    }
}

impl ConvolutionContext {
    /// Builds the convolution effect for `effect` (a [`BusEffect::Convolution`]) and hands its
    /// builder to `add`; returns the game thread's handle. An unknown IR, or the low tier, makes
    /// an effect that passes only the dry part of the mix. `send` says the effect is built on a
    /// send track: it selects the neutral fallback for a non-finite mix (fully wet on a send —
    /// its input is the send — dry on an ordinary bus).
    fn add_effect(
        &self,
        effect: &BusEffect,
        send: bool,
        add: impl FnOnce(crate::ConvolutionEffectBuilder),
    ) -> crate::ConvolutionHandle {
        let BusEffect::Convolution {
            ir,
            mix,
            predelay_secs,
        } = finite_effect(effect, send)
        else {
            let (builder, handle) = crate::ConvolutionEffectBuilder::new(self.sample_rate, None, 0.0, None);
            add(builder);
            return handle;
        };
        let kernel = if self.tier == crate::ConvolutionTier::Low {
            None
        } else {
            let kernel = self.kernels.get(&ir).cloned();
            if kernel.is_none() && !ir.is_empty() {
                crate::sound::note_rejected("convolution IR");
                tracing::warn!(target: "regia_audio", "bus convolution: IR `{ir}` is not registered; dry only");
            }
            kernel
        };
        let slot = kernel.map(|k| crate::ConvolutionSlot::new(k, predelay_secs));
        let (builder, mut handle) =
            crate::ConvolutionEffectBuilder::new(self.sample_rate, slot, mix, Some(&ir));
        handle.note_predelay(predelay_secs);
        add(builder);
        handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kira::Tweenable;

    #[test]
    fn a_reader_without_the_tap_field_ignores_it() {
        // A v0.6 file with a pre-fader send, read by a build whose `BusSend` predates the tap
        // (the v1 shape before the fold, ADR-8590): the field is skipped and the send loads.
        #[derive(Debug, Deserialize)]
        struct OldSend {
            bus: String,
            volume_db: f32,
        }
        let text = "(bus: \"Reverb\", volume_db: -6.0, tap: PreFader)";
        let old: OldSend = ron::from_str(text).expect("an older reader skips the tap");
        assert_eq!((old.bus.as_str(), old.volume_db), ("Reverb", -6.0));
        let new: BusSend = ron::from_str(text).expect("loads");
        assert_eq!(new, BusSend::new("Reverb", -6.0).with_tap(SendTap::PreFader));
        let default: BusSend = ron::from_str("(bus: \"Reverb\", volume_db: -6.0)").expect("loads");
        assert_eq!(default.tap, SendTap::PostFader);
    }

    #[test]
    fn the_schema_fingerprint_names_the_tap() {
        let fingerprint = bus_layout_schema_fingerprint();
        assert!(fingerprint.contains("tap"), "{fingerprint}");
        assert!(fingerprint.contains("PreFader"), "{fingerprint}");
    }

    #[test]
    fn safe_duration_never_panics() {
        assert_eq!(safe_duration(-1.0), std::time::Duration::ZERO);
        assert_eq!(safe_duration(f32::NAN), std::time::Duration::ZERO);
        assert_eq!(safe_duration(f32::INFINITY), std::time::Duration::ZERO);
        assert_eq!(safe_duration(1.0e30), std::time::Duration::from_secs(3_600));
        assert_eq!(safe_duration(0.5), std::time::Duration::from_millis(500));
    }

    #[test]
    fn validate_rejects_bad_effect_times() {
        for effect in [
            BusEffect::Delay {
                delay_secs: -1.0,
                feedback_db: -6.0,
                mix: 0.5,
            },
            BusEffect::Compressor {
                threshold_db: -12.0,
                ratio: 2.0,
                attack_secs: f32::NAN,
                release_secs: 0.1,
                makeup_db: 0.0,
            },
            BusEffect::Compressor {
                threshold_db: -12.0,
                ratio: 2.0,
                attack_secs: 0.01,
                release_secs: f32::INFINITY,
                makeup_db: 0.0,
            },
        ] {
            let mut layout = BusLayout::default_tree();
            layout.buses[1].effects.push(effect);
            assert!(layout.validate().is_err(), "bad effect time must be rejected");
        }
        let mut ok = BusLayout::default_tree();
        ok.buses[1].effects.push(BusEffect::Delay {
            delay_secs: 0.25,
            feedback_db: -6.0,
            mix: 0.5,
        });
        ok.validate().expect("a sane delay validates");
    }

    #[test]
    fn mute_target_is_finite_so_tweens_never_interpolate_an_infinity() {
        // kira lerps decibels as `a + (b - a) * t`; an infinite end evaluates to NaN.
        assert!(MUTE_DB.is_finite());
        let lerped = Tweenable::interpolate(Decibels(0.0), Decibels(MUTE_DB), 0.5);
        assert!(lerped.0.is_finite());
        let from_mute = Tweenable::interpolate(Decibels(MUTE_DB), Decibels(-3.0), 0.0);
        assert_eq!(from_mute.0, MUTE_DB);
    }

    #[test]
    fn default_tree_matches_the_spec() {
        let layout = BusLayout::default_tree();
        layout.validate().expect("the default tree is valid");
        let names: Vec<&str> = layout.buses.iter().map(|b| b.name.as_str()).collect();
        for expected in ["Master", "Music", "Sfx", "World", "Ui", "Voice", "Ambience"] {
            assert!(names.contains(&expected), "missing bus {expected}");
        }
        let master = &layout.buses[0];
        assert_eq!(master.parent, None);
        let sfx = layout.buses.iter().find(|b| b.name == "Sfx").expect("Sfx exists");
        assert_eq!(sfx.parent.as_deref(), Some("Master"));
        let world = layout
            .buses
            .iter()
            .find(|b| b.name == "World")
            .expect("World exists");
        assert_eq!(world.parent.as_deref(), Some("Sfx"));
        assert_eq!(world.sends.len(), 1);
        assert_eq!(world.sends[0].bus, "Reverb");
    }

    #[test]
    fn layout_validation_rejects_forward_parents_and_cycles() {
        let mut layout = BusLayout::default_tree();
        layout.buses[1].parent = Some("Ui".into()); // Ui is defined later
        assert!(layout.validate().is_err());

        let mut layout = BusLayout::default_tree();
        layout.buses[0].parent = Some("Master".into()); // self-parent
        assert!(layout.validate().is_err());
    }

    #[test]
    fn bus_levels_stay_finite() {
        assert_eq!(finite_db(f32::NEG_INFINITY), MUTE_DB);
        assert_eq!(finite_db(f32::NAN), MUTE_DB);
        assert_eq!(finite_db(f32::INFINITY), MAX_BUS_DB);
        assert_eq!(finite_db(-6.0), -6.0);
        assert!(
            crate::spatial::db_to_gain(MUTE_DB) == 0.0,
            "the mute floor is silence"
        );
    }

    #[test]
    fn convolution_build_sends_the_send_context_to_the_nan_fallback() {
        // A NaN mix is neutralised at build: dry passthrough on an ordinary bus, fully wet on a
        // send bus (a send track's input IS the send; a dry fallback would pass the tapped
        // signal straight into the main mix, doubling it).
        let context = ConvolutionContext::default();
        let effect = BusEffect::Convolution {
            ir: String::new(),
            mix: f32::NAN,
            predelay_secs: 0.0,
        };
        let send = context.add_effect(&effect, true, |_| {});
        assert_eq!(send.stats().mix, 1.0, "a send-bus NaN mix falls back to wet");
        let bus = context.add_effect(&effect, false, |_| {});
        assert_eq!(bus.stats().mix, 0.0, "an ordinary-bus NaN mix falls back to dry");
    }

    #[test]
    fn ron_round_trip() {
        let layout = BusLayout::default_tree();
        let text = ron::to_string(&layout).expect("serialize");
        let back = BusLayout::from_ron(&text).expect("parse");
        assert_eq!(layout, back);
    }
}
