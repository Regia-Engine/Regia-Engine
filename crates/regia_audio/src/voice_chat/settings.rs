// SPDX-License-Identifier: MPL-2.0
//! Voice chat configuration (`config/voice_chat.ron`, format version 1) and the persisted user
//! settings (`docs/specs/59-voice-chat.md` §§ 3, 4).

use super::error::VoiceChatError;
use super::jitter::JitterSettings;
use super::moderation::AccountList;
use super::packet::VoiceQuality;
use super::transmit::{FrameVadSettings, VoiceMode};
use regia_ecs::prelude::*;
use ron::value::RawValue;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Version of `config/voice_chat.ron` written by this build.
pub const CONFIG_FORMAT_VERSION: u32 = 1;
/// Channel id (stable across a project's lifetime; part of the wire format).
pub type VoiceChannelId = u16;
/// Reserved sender id of the local loopback speaker.
pub const LOOPBACK_SENDER: u16 = 0xFFFF;
/// Frames after which loopback turns itself off (60 s).
pub const LOOPBACK_MAX_FRAMES: u64 = 3000;

/// Kind of a channel (spec § 2.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VoiceChannelKind {
    /// Players on the same team, flat stereo.
    #[default]
    Team,
    /// Players within a radius of the speaker, spatialized.
    Proximity,
    /// An explicit member list that survives level changes.
    Party,
    /// Every player in the session.
    Global,
    /// Game-defined.
    Custom,
}

impl VoiceChannelKind {
    /// Lowercase name for the agent protocol.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Team => "team",
            Self::Proximity => "proximity",
            Self::Party => "party",
            Self::Global => "global",
            Self::Custom => "custom",
        }
    }
}

/// A named audience.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceChannel {
    /// Stable channel id.
    pub id: VoiceChannelId,
    /// Display name.
    pub name: String,
    /// Kind.
    pub kind: VoiceChannelKind,
    /// Audible radius in metres (proximity and spatial custom channels).
    pub radius: f32,
    /// Play through the spatial path.
    pub spatial: bool,
    /// Priority class for the speaker cap (higher wins).
    pub priority: u8,
}

impl Default for VoiceChannel {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            kind: VoiceChannelKind::Team,
            radius: 25.0,
            spatial: false,
            priority: 0,
        }
    }
}

/// Noise suppression level (spec § 2.7). `Strong` needs the unbuilt `voice-denoise-rnn` feature
/// and reports as `Light`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoiseSuppression {
    /// Off.
    #[default]
    Off,
    /// Spectral gate.
    Light,
    /// Recurrent network (feature `voice-denoise-rnn`).
    Strong,
}

/// Optional processing stages (spec § 2.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcessingSettings {
    /// Noise suppression level.
    pub noise_suppression: NoiseSuppression,
    /// Echo cancellation.
    pub echo_cancel: bool,
    /// Automatic gain.
    pub agc: bool,
}

/// Project defaults, `config/voice_chat.ron`.
#[derive(Resource, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceChatSettings {
    /// File format version.
    pub format_version: u32,
    /// Bandwidth profile.
    pub quality: VoiceQuality,
    /// Transmit mode.
    pub mode: VoiceMode,
    /// Frames per packet, 1 or 2.
    pub frames_per_packet: u8,
    /// Speakers decoded and mixed at once, 1..=16.
    pub max_mixed_speakers: u8,
    /// Jitter buffer bounds.
    pub jitter: JitterSettings,
    /// Processing stages.
    pub processing: ProcessingSettings,
    /// Activity detector.
    pub vad: FrameVadSettings,
    /// Opus complexity, 0..=10.
    pub complexity: u8,
    /// Append the previous frame when loss is high.
    pub redundancy: bool,
    /// Seconds of encoded packets kept for reports, 0..=30 (0 = never keep).
    pub report_buffer_seconds: u32,
    /// Deterministic test mode: tick clock, no time scaling, no processing.
    pub deterministic: bool,
    /// Channel definitions.
    pub channels: Vec<VoiceChannel>,
}

impl Default for VoiceChatSettings {
    fn default() -> Self {
        let ch = |id, name: &str, kind, radius, spatial, priority| VoiceChannel {
            id,
            name: name.to_string(),
            kind,
            radius,
            spatial,
            priority,
        };
        Self {
            format_version: CONFIG_FORMAT_VERSION,
            quality: VoiceQuality::Standard,
            mode: VoiceMode::PushToTalk,
            frames_per_packet: 1,
            max_mixed_speakers: 8,
            jitter: JitterSettings::default(),
            processing: ProcessingSettings::default(),
            vad: FrameVadSettings::default(),
            complexity: 5,
            redundancy: false,
            report_buffer_seconds: 0,
            deterministic: false,
            channels: vec![
                ch(1, "team", VoiceChannelKind::Team, 25.0, false, 3),
                ch(2, "nearby", VoiceChannelKind::Proximity, 25.0, true, 2),
                ch(3, "party", VoiceChannelKind::Party, 25.0, false, 1),
                ch(4, "all", VoiceChannelKind::Global, 25.0, false, 0),
            ],
        }
    }
}

impl VoiceChatSettings {
    /// Checks every range.
    ///
    /// # Errors
    /// [`VoiceChatError::InvalidParams`] naming the field.
    pub fn validate(&self) -> Result<(), VoiceChatError> {
        let bad = |m: &str| Err(VoiceChatError::InvalidParams(m.to_string()));
        if !(1..=16).contains(&self.max_mixed_speakers) {
            return bad("max_mixed_speakers must be in 1..=16");
        }
        if !(1..=2).contains(&self.frames_per_packet) {
            return bad("frames_per_packet must be 1 or 2");
        }
        if self.frames_per_packet == 2 && self.redundancy {
            // A two-frame packet has no slot for a spare frame: the wire format carries the
            // redundancy bit only on the first of exactly two frames, so the combination would
            // silently send without redundancy.
            return bad("redundancy needs frames_per_packet 1");
        }
        if self.complexity > 10 {
            return bad("complexity must be in 0..=10");
        }
        if self.report_buffer_seconds > 30 {
            return bad("report_buffer_seconds must be in 0..=30");
        }
        let j = &self.jitter;
        if j.min_ms == 0 || j.min_ms > j.start_ms || j.start_ms > j.max_ms {
            return bad("jitter needs 0 < min_ms <= start_ms <= max_ms");
        }
        let mut ids: Vec<u16> = self.channels.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        if ids.windows(2).any(|w| w[0] == w[1]) {
            return bad("channel ids must be unique");
        }
        if self
            .channels
            .iter()
            .any(|c| !c.radius.is_finite() || c.radius < 0.0)
        {
            return bad("channel radius must be finite and not negative");
        }
        Ok(())
    }

    /// Parses `config/voice_chat.ron`, then validates it. Version-1 files only; a newer
    /// version is refused rather than guessed at.
    ///
    /// # Errors
    /// [`VoiceChatError::InvalidParams`] with the parse or range problem.
    pub fn from_ron(text: &str) -> Result<Self, VoiceChatError> {
        let s: Self =
            ron::from_str(text).map_err(|e| VoiceChatError::InvalidParams(format!("voice_chat.ron: {e}")))?;
        if s.format_version != CONFIG_FORMAT_VERSION {
            return Err(VoiceChatError::InvalidParams(format!(
                "voice_chat.ron format_version {} is not supported (this build reads {CONFIG_FORMAT_VERSION})",
                s.format_version
            )));
        }
        s.validate()?;
        Ok(s)
    }

    /// Serializes to RON text.
    ///
    /// # Errors
    /// [`VoiceChatError::InvalidParams`] if serialization fails.
    pub fn to_ron(&self) -> Result<String, VoiceChatError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| VoiceChatError::InvalidParams(format!("voice_chat.ron: {e}")))
    }
}

/// The `from` versions of the `config/voice_chat.ron` migration steps (none at v1), for
/// `cargo xtask check-formats`.
#[must_use]
pub fn voice_chat_config_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape fingerprint of `config/voice_chat.ron` (hashed by `cargo xtask check-formats`):
/// the field list of every struct reachable from [`VoiceChatSettings`], then its enum spellings.
#[must_use]
pub fn voice_chat_config_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<VoiceChatSettings>(),
        serde_field_names::<JitterSettings>(),
        serde_field_names::<ProcessingSettings>(),
        serde_field_names::<FrameVadSettings>(),
        serde_field_names::<VoiceChannel>(),
        serde_variant_names::<VoiceQuality>(),
        serde_variant_names::<VoiceMode>(),
        serde_variant_names::<NoiseSuppression>(),
        serde_variant_names::<VoiceChannelKind>(),
    ])
}

/// The persisted per-user subset (spec § 4). Fresh profiles have no consent and push-to-talk.
///
/// Unknown keys round-trip (spec § 4): a key this build does not know is kept as its raw RON
/// text in [`unknown_keys`](Self::unknown_keys) and written back on save, so a newer build's file
/// survives an older build. Missing keys take their defaults.
#[derive(Resource, Clone, Debug, PartialEq)]
pub struct VoiceChatUserSettings {
    /// Microphone consent; the device is never opened while this is `false`.
    pub consent: bool,
    /// Stable device name, `None` for the system default.
    pub device: Option<String>,
    /// Transmit mode.
    pub mode: VoiceMode,
    /// Input gain in dB.
    pub input_gain_db: f32,
    /// Output gain in dB.
    pub output_gain_db: f32,
    /// Muted accounts.
    pub muted_accounts: AccountList,
    /// Blocked accounts.
    pub blocked_accounts: AccountList,
    /// Deafen (silences all playback, implies self mute).
    pub deafen: bool,
    /// Keys a newer build wrote, kept verbatim for the next save.
    pub unknown_keys: UnknownKeys,
}

impl Default for VoiceChatUserSettings {
    fn default() -> Self {
        Self {
            consent: false,
            device: None,
            mode: VoiceMode::PushToTalk,
            input_gain_db: 0.0,
            output_gain_db: 0.0,
            muted_accounts: AccountList::default(),
            blocked_accounts: AccountList::default(),
            deafen: false,
            unknown_keys: UnknownKeys::default(),
        }
    }
}

/// The field names of [`VoiceChatUserSettings`] this build knows, in file order.
const USER_FIELDS: &[&str] = &[
    "consent",
    "device",
    "mode",
    "input_gain_db",
    "output_gain_db",
    "muted_accounts",
    "blocked_accounts",
    "deafen",
];

/// Most unknown keys one settings section keeps (more are dropped with a warning).
pub const MAX_UNKNOWN_KEYS: usize = 64;

/// Unknown keys of a settings section and their raw RON values, in key order.
///
/// Only RON carries them: under another serde format (JSON in agent output) an unknown key is
/// ignored on read and its raw RON text is written as a string.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UnknownKeys(BTreeMap<&'static str, Box<RawValue>>);

impl UnknownKeys {
    /// Number of kept keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no key is kept.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The raw RON text of `key`, if kept.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|v| v.get_ron())
    }

    /// The kept key names, in order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().copied()
    }
}

/// Unknown key names are interned (serde's struct serializer needs `&'static str` field names).
/// Each distinct name is stored once per process, and at most this many names are ever interned.
const MAX_INTERNED_KEYS: usize = 1024;

fn intern_key(key: &str) -> Option<&'static str> {
    use std::collections::BTreeSet;
    use std::sync::{Mutex, OnceLock};
    static NAMES: OnceLock<Mutex<BTreeSet<&'static str>>> = OnceLock::new();
    let mut names = NAMES
        .get_or_init(|| Mutex::new(BTreeSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(found) = names.get(key) {
        return Some(found);
    }
    if names.len() >= MAX_INTERNED_KEYS {
        return None;
    }
    let leaked: &'static str = Box::leak(key.to_owned().into_boxed_str());
    names.insert(leaked);
    Some(leaked)
}

/// An unknown value: the raw RON text under the RON deserializer, nothing under any other.
struct UnknownValue(Option<Box<RawValue>>);

/// The newtype name the RON deserializer answers with the raw text of the value (`ron` 0.12
/// `value::raw`; checked by `unknown_keys_round_trip_through_ron`, which fails if a `ron`
/// upgrade changes it).
const RON_RAW_VALUE_TOKEN: &str = "$ron::private::RawValue";

impl<'de> Deserialize<'de> for UnknownValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = UnknownValue;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("any value")
            }

            // The RON deserializer hands over the raw text, with any comment around it; a
            // forwarding foreign deserializer hands over a plain string, kept only if it is RON.
            fn visit_str<E: serde::de::Error>(self, ron: &str) -> Result<UnknownValue, E> {
                match RawValue::from_boxed_ron(ron.into()) {
                    Ok(v) => Ok(UnknownValue(Some(v.trim_boxed()))),
                    Err(e) => {
                        tracing::warn!(target: "regia_audio", "voice_chat settings: an unknown key is not kept: {e}");
                        Ok(UnknownValue(None))
                    }
                }
            }

            // Any other shape comes from a foreign deserializer: ignore it.
            fn visit_newtype_struct<D: serde::Deserializer<'de>>(
                self,
                d: D,
            ) -> Result<UnknownValue, D::Error> {
                serde::de::IgnoredAny::deserialize(d)?;
                Ok(UnknownValue(None))
            }

            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<UnknownValue, E> {
                Ok(UnknownValue(None))
            }

            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<UnknownValue, E> {
                Ok(UnknownValue(None))
            }

            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<UnknownValue, E> {
                Ok(UnknownValue(None))
            }

            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<UnknownValue, E> {
                Ok(UnknownValue(None))
            }

            fn visit_bytes<E: serde::de::Error>(self, _: &[u8]) -> Result<UnknownValue, E> {
                Ok(UnknownValue(None))
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<UnknownValue, E> {
                Ok(UnknownValue(None))
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<UnknownValue, E> {
                Ok(UnknownValue(None))
            }

            fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<UnknownValue, D::Error> {
                serde::de::IgnoredAny::deserialize(d)?;
                Ok(UnknownValue(None))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<UnknownValue, A::Error> {
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(UnknownValue(None))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<UnknownValue, A::Error> {
                while map
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(UnknownValue(None))
            }
        }
        deserializer.deserialize_newtype_struct(RON_RAW_VALUE_TOKEN, Visit)
    }
}

impl Serialize for VoiceChatUserSettings {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = serializer.serialize_struct(
            "VoiceChatUserSettings",
            USER_FIELDS.len() + self.unknown_keys.len(),
        )?;
        st.serialize_field("consent", &self.consent)?;
        st.serialize_field("device", &self.device)?;
        st.serialize_field("mode", &self.mode)?;
        st.serialize_field("input_gain_db", &self.input_gain_db)?;
        st.serialize_field("output_gain_db", &self.output_gain_db)?;
        st.serialize_field("muted_accounts", &self.muted_accounts)?;
        st.serialize_field("blocked_accounts", &self.blocked_accounts)?;
        st.serialize_field("deafen", &self.deafen)?;
        for (&key, value) in &self.unknown_keys.0 {
            st.serialize_field(key, value)?;
        }
        st.end()
    }
}

impl<'de> Deserialize<'de> for VoiceChatUserSettings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = VoiceChatUserSettings;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("the voice_chat user settings")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<VoiceChatUserSettings, A::Error> {
                let mut out = VoiceChatUserSettings::default();
                let mut warned = false;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "consent" => out.consent = map.next_value()?,
                        "device" => out.device = map.next_value()?,
                        "mode" => out.mode = map.next_value()?,
                        "input_gain_db" => out.input_gain_db = map.next_value()?,
                        "output_gain_db" => out.output_gain_db = map.next_value()?,
                        "muted_accounts" => out.muted_accounts = map.next_value()?,
                        "blocked_accounts" => out.blocked_accounts = map.next_value()?,
                        "deafen" => out.deafen = map.next_value()?,
                        _ => {
                            let UnknownValue(raw) = map.next_value()?;
                            let Some(raw) = raw else { continue };
                            let kept = if out.unknown_keys.len() < MAX_UNKNOWN_KEYS {
                                intern_key(&key)
                            } else {
                                None
                            };
                            match kept {
                                Some(name) => {
                                    out.unknown_keys.0.insert(name, raw);
                                }
                                None if !warned => {
                                    warned = true;
                                    tracing::warn!(
                                        target: "regia_audio",
                                        "voice_chat settings: too many unknown keys; `{key}` and later ones are not kept"
                                    );
                                }
                                None => {}
                            }
                        }
                    }
                }
                Ok(out)
            }
        }
        deserializer.deserialize_struct("VoiceChatUserSettings", USER_FIELDS, Visit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file a newer build wrote: unknown keys of every RON shape survive a load and a save,
    /// and the known keys parse as before.
    #[test]
    fn unknown_keys_round_trip_through_ron() {
        let newer = r#"(
            consent: true,
            device: Some("USB mic"),
            mode: Open,
            future_level: Fancy(level: 3, tags: ["a", "b"]),
            future_list: [1, 2, 3],
            future_enum: Strong,
            future_map: {"k": (x: 1.5)},
            deafen: true,
        )"#;
        let user: VoiceChatUserSettings = ron::from_str(newer).expect("parses");
        assert!(user.consent && user.deafen);
        assert_eq!(user.device.as_deref(), Some("USB mic"));
        assert_eq!(user.mode, VoiceMode::Open);
        assert_eq!(
            user.unknown_keys.keys().collect::<Vec<_>>(),
            vec!["future_enum", "future_level", "future_list", "future_map"]
        );
        assert_eq!(
            user.unknown_keys.get("future_level"),
            Some(r#"Fancy(level: 3, tags: ["a", "b"])"#)
        );
        let saved = ron::ser::to_string_pretty(&user, regia_core::ron_pretty()).expect("saves");
        let again: VoiceChatUserSettings = ron::from_str(&saved).expect("reloads");
        assert_eq!(again, user);
        assert!(
            saved.contains("future_level: Fancy(level: 3, tags: [\"a\", \"b\"])"),
            "{saved}"
        );
    }

    /// A hand-edited file with comments around an unknown value still loads, and saves without
    /// the comment eating the separator.
    #[test]
    fn unknown_values_with_comments_load() {
        let text = "(\n    consent: true,\n    future_x: 5 // a note\n)";
        let user: VoiceChatUserSettings = ron::from_str(text).expect("parses");
        assert!(user.consent);
        assert_eq!(user.unknown_keys.get("future_x"), Some("5"));
        let saved = ron::to_string(&user).expect("saves");
        assert_eq!(
            ron::from_str::<VoiceChatUserSettings>(&saved).expect("reloads"),
            user
        );
    }

    /// Missing keys take their defaults; an old file without unknown keys writes no extra keys.
    #[test]
    fn missing_keys_default_and_old_files_are_unchanged() {
        let user: VoiceChatUserSettings = ron::from_str("(consent: true)").expect("parses");
        assert_eq!(
            user,
            VoiceChatUserSettings {
                consent: true,
                ..VoiceChatUserSettings::default()
            }
        );
        let defaults: VoiceChatUserSettings = ron::from_str("()").expect("parses");
        assert_eq!(defaults, VoiceChatUserSettings::default());
        let saved = ron::to_string(&VoiceChatUserSettings::default()).expect("saves");
        let back: VoiceChatUserSettings = ron::from_str(&saved).expect("reloads");
        assert!(back.unknown_keys.is_empty());
    }

    /// Under another serde format unknown keys never fail the parse: values that are not
    /// strings are ignored (serde's own map deserializer stands in for a foreign format; JSON
    /// takes the newtype path and ignores strings too, while a forwarding deserializer hands a
    /// string over as text, which is kept only if it is valid RON).
    #[test]
    fn foreign_formats_ignore_unknown_keys() {
        use serde::de::value::{Error, MapDeserializer};
        let de = MapDeserializer::<_, Error>::new([("mode", "Vad")].into_iter());
        assert_eq!(
            VoiceChatUserSettings::deserialize(de).expect("parses").mode,
            VoiceMode::Vad
        );
        let de = MapDeserializer::<_, Error>::new([("future", 5u32), ("other", 7)].into_iter());
        let user = VoiceChatUserSettings::deserialize(de).expect("parses");
        assert!(user.unknown_keys.is_empty());
        let de = MapDeserializer::<_, Error>::new([("future", true)].into_iter());
        assert!(
            VoiceChatUserSettings::deserialize(de)
                .expect("parses")
                .unknown_keys
                .is_empty()
        );
    }

    /// A hostile file with more unknown keys than the cap keeps the cap and still loads.
    #[test]
    fn unknown_keys_are_capped() {
        let body: Vec<String> = (0..MAX_UNKNOWN_KEYS + 10).map(|i| format!("k{i}: {i}")).collect();
        let text = format!("({})", body.join(", "));
        let user: VoiceChatUserSettings = ron::from_str(&text).expect("parses");
        assert_eq!(user.unknown_keys.len(), MAX_UNKNOWN_KEYS);
    }

    /// `redundancy` attaches a spare copy as the first of two frames; a two-frame packet has no
    /// slot for it, so the combination is refused instead of silently sending without
    /// redundancy (correctness audit finding 7).
    #[test]
    fn redundancy_with_two_frame_packets_is_rejected() {
        let good = VoiceChatSettings {
            redundancy: true,
            ..Default::default()
        };
        good.validate().expect("one-frame packets can carry a spare");
        let bad = VoiceChatSettings {
            redundancy: true,
            frames_per_packet: 2,
            ..Default::default()
        };
        let error = bad.validate().expect_err("no slot for a spare frame");
        assert!(
            error.to_string().contains("redundancy"),
            "the error names the field: {error}"
        );
    }
}
