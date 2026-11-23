// SPDX-License-Identifier: MPL-2.0
//! Settings of the audio system.

use crate::bus::BusLayout;
use crate::ducking::{DuckRule, default_duck_rules};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

/// Resource read by [`AudioPlugin`](crate::AudioPlugin) when it builds. Insert before adding the
/// plugin to change buses, ducking rules, the voice limit or to force the offline backend
/// (tests, CI, dedicated servers).
#[derive(Resource, Clone, Debug, PartialEq)]
pub struct AudioSettings {
    /// Force the offline backend even when an output device is available.
    pub headless: bool,
    /// The bus tree to build.
    pub buses: BusLayout,
    /// Ducking rules.
    pub duck_rules: Vec<DuckRule>,
    /// Maximum real voices; `None` picks the platform default (64 desktop, 32 mobile).
    pub voice_limit: Option<usize>,
    /// Amplitude below which a voice is virtualised, as dB (−48 ≈ inaudible).
    pub virtual_threshold_db: f32,
    /// Sample rate the offline backend runs at.
    pub sample_rate: u32,
}

/// Lowest rate the offline backend runs at, Hz. kira sizes reverb and delay buffers from the
/// rate, and below about 200 Hz they come out zero frames long and the mixer thread panics; the
/// same floor as a render-out mixdown (`MIXDOWN_SAMPLE_RATES`).
pub const MIN_OFFLINE_SAMPLE_RATE: u32 = 8_000;

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            headless: false,
            buses: BusLayout::default_tree(),
            duck_rules: default_duck_rules(),
            voice_limit: None,
            virtual_threshold_db: -48.0,
            sample_rate: 48_000,
        }
    }
}

/// The platform default voice limit.
#[must_use]
pub fn default_voice_limit() -> usize {
    if cfg!(target_os = "android") { 32 } else { 64 }
}

/// User-facing volume sliders (spec § 1): which buses the settings screen exposes by default.
pub const SETTINGS_BUSES: [&str; 5] = ["Master", "Music", "Sfx", "Voice", "Ambience"];

/// RON form of the user's audio settings (the settings-screen volumes and mutes, and the voice
/// chat section). A game persists and reloads this itself (its own settings file); it is not
/// `config/audio.ron` — that file is the [`BusLayout`] bus tree (spec 16 § 1), read by the
/// runtime and the editor mixer (ADR-5699).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioUserSettings {
    /// Bus name → volume in dB.
    pub bus_volumes: std::collections::HashMap<String, f32>,
    /// Muted buses.
    pub muted: Vec<String>,
    /// The voice chat section (specs/59 § 4): consent, device, mode, gains, mutes and blocks.
    #[serde(default)]
    pub voice_chat: crate::voice_chat::VoiceChatUserSettings,
}
