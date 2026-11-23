// SPDX-License-Identifier: MPL-2.0
//! The gameplay pause of the mixer (ADR-10105): while `regia_app::Time::paused` is set, every
//! voice on a game bus holds in place and resumes where it stopped.
//!
//! [`Audio::set_game_paused`] is called by the `AudioSync` system every frame with
//! `Time::paused`. Entering the pause holds every live voice whose bus is not exempt
//! ([`Audio::set_pause_exempt_buses`], default `Ui` and `Music`): the handle pauses with the
//! declick fade and the voice is marked [`VoiceEntry::held`](crate::VoiceEntry::held). A
//! voice that starts on a frozen bus while paused is held on the frame it starts. Leaving the pause
//! resumes exactly the voices the pause held; a voice the host paused itself
//! ([`Audio::pause_voice`]) stays paused. Voice chat is never held (it is people talking, not the
//! game world).
//!
//! While held, a voice's age and virtual cursor do not advance, an emitter-track rebuild or a
//! virtualise/revive round trip re-applies the hold, and the world parts of the audio tick
//! (propagation fades, patch layers) take a zero step. The mixer bookkeeping (ducking, snapshot
//! blends, stream watch, voice chat) keeps running on the real frame time, so a pause screen's
//! snapshot and its UI sounds work.

use std::time::Duration;

use super::Audio;
use crate::voices::VoiceKind;

/// The fade a held voice pauses and resumes with (the declick minimum: `instance::DECLICK_FADE`).
pub const GAME_PAUSE_FADE: Duration = Duration::from_millis(10);

/// The buses a gameplay pause leaves playing by default: the UI (the pause screen's own sounds)
/// and the soundtrack.
pub const DEFAULT_PAUSE_EXEMPT_BUSES: [&str; 2] = ["Ui", "Music"];

/// The gameplay-pause state of the mixer.
#[derive(Debug, Clone)]
pub(crate) struct GamePauseState {
    /// `Time::paused` as of the last [`Audio::set_game_paused`].
    pub(crate) paused: bool,
    /// Buses (with their child buses) that keep playing while paused.
    pub(crate) exempt: Vec<String>,
    /// Voices held by the current pause, counted since start.
    pub(crate) held_total: u64,
}

impl Default for GamePauseState {
    fn default() -> Self {
        Self {
            paused: false,
            exempt: DEFAULT_PAUSE_EXEMPT_BUSES
                .iter()
                .map(|bus| (*bus).to_owned())
                .collect(),
            held_total: 0,
        }
    }
}

impl Audio {
    /// Sets the gameplay pause (`Time::paused`; the `AudioSync` system calls this every frame).
    /// Paused: every voice on a bus the pause freezes holds in place (declick fade). Unpaused: the
    /// held voices resume from where they stopped. Calling it again while paused holds the voices
    /// that started since.
    pub fn set_game_paused(&mut self, paused: bool) {
        let was = self.game_pause.paused;
        self.game_pause.paused = paused;
        if paused {
            let mut held = 0u64;
            let layout = self.buses.layout();
            let exempt = &self.game_pause.exempt;
            for entry in &mut self.voices.entries {
                if entry.held || entry.stopping || entry.kind == VoiceKind::VoiceChat {
                    continue;
                }
                if !freezes(layout, exempt, &entry.bus) {
                    continue;
                }
                entry.held = true;
                held += 1;
                if !entry.paused
                    && let Some(instance) = entry.instance.as_mut()
                {
                    instance.pause(GAME_PAUSE_FADE);
                }
            }
            self.game_pause.held_total += held;
        } else if was {
            for entry in &mut self.voices.entries {
                if !std::mem::take(&mut entry.held) {
                    continue;
                }
                if !entry.paused
                    && let Some(instance) = entry.instance.as_mut()
                {
                    instance.resume(GAME_PAUSE_FADE);
                }
            }
        }
    }

    /// Whether the gameplay pause is on (the last [`Self::set_game_paused`]).
    #[must_use]
    pub fn game_paused(&self) -> bool {
        self.game_pause.paused
    }

    /// Sets the buses a gameplay pause leaves playing (each with its child buses). The default is
    /// [`DEFAULT_PAUSE_EXEMPT_BUSES`]; an empty list freezes every bus but voice chat. Takes effect
    /// at the next pause.
    pub fn set_pause_exempt_buses<I, S>(&mut self, buses: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.game_pause.exempt = buses.into_iter().map(Into::into).collect();
    }

    /// The buses a gameplay pause leaves playing ([`Self::set_pause_exempt_buses`]).
    #[must_use]
    pub fn pause_exempt_buses(&self) -> &[String] {
        &self.game_pause.exempt
    }

    /// Voices held by the gameplay pause right now.
    #[must_use]
    pub fn held_voice_count(&self) -> usize {
        self.voices.entries.iter().filter(|entry| entry.held).count()
    }

    /// Voices the gameplay pause held since start (a counter for tests and the agent).
    #[must_use]
    pub fn held_voice_total(&self) -> u64 {
        self.game_pause.held_total
    }

    /// Whether a gameplay pause freezes voices on `bus`: neither the bus nor any of its ancestors
    /// is exempt. A bus the layout does not know freezes unless it is exempt by name.
    #[must_use]
    pub fn pause_freezes_bus(&self, bus: &str) -> bool {
        freezes(self.buses.layout(), &self.game_pause.exempt, bus)
    }

    /// Holds a voice that just started, when the gameplay pause is on and its bus freezes. A
    /// fresh handle has not been heard, so it pauses without a fade.
    pub(super) fn hold_new_voice(&mut self, id: crate::SoundInstanceId) {
        if !self.game_pause.paused {
            return;
        }
        let Some(entry) = self.voices.get(id) else {
            return;
        };
        if entry.kind == VoiceKind::VoiceChat || !self.pause_freezes_bus(&entry.bus) {
            return;
        }
        self.game_pause.held_total += 1;
        if let Some(entry) = self.voices.get_mut(id) {
            entry.held = true;
            if !entry.paused
                && let Some(instance) = entry.instance.as_mut()
            {
                instance.pause(Duration::ZERO);
            }
        }
    }

    /// The step the world parts of the tick take: zero while the gameplay pause is on.
    pub(super) fn world_step(&self, dt: f64) -> f64 {
        if self.game_pause.paused { 0.0 } else { dt }
    }
}

/// Whether a pause freezes `bus` under `layout` with `exempt` buses: neither the bus nor an ancestor
/// is exempt.
fn freezes(layout: &crate::bus::BusLayout, exempt: &[String], bus: &str) -> bool {
    let mut current = Some(bus);
    // A validated layout's parent chain is acyclic; the bound guards a raw one.
    for _ in 0..=layout.buses.len() {
        let Some(name) = current else {
            return true;
        };
        if exempt.iter().any(|exempt| exempt == name) {
            return false;
        }
        current = layout
            .buses
            .iter()
            .find(|def| def.name == name)
            .and_then(|def| def.parent.as_deref());
    }
    true
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kira::sound::PlaybackState;

    use super::*;
    use crate::{AudioSettings, PlaybackParams, SoundAsset};

    fn headless() -> Audio {
        Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        })
    }

    fn tone(len: usize) -> SoundAsset {
        let clip = crate::SoundClip::new(vec![0.1; len], 48_000).expect("clip");
        SoundAsset::from_clip(&clip)
    }

    fn on(bus: &str) -> PlaybackParams {
        PlaybackParams {
            bus: Some(bus.into()),
            ..PlaybackParams::default()
        }
    }

    /// The bar: a world voice's playback position does not move across a paused interval, a UI
    /// voice keeps playing, and the world voice resumes where it stopped.
    #[test]
    fn a_world_voice_holds_its_position_while_paused_and_a_ui_voice_plays_on() {
        let mut audio = headless();
        let long = tone(48_000 * 10);
        let world = audio.start_asset(&long, on("World")).expect("world voice");
        let ui = audio.start_asset(&long, on("Ui")).expect("ui voice");
        let _ = audio.render_offline(4_800);
        audio.set_game_paused(true);
        // Let the declick fade finish and its last position reach the handle (a handle reports
        // the state of the block before), then measure.
        let _ = audio.render_offline(4_800);
        let _ = audio.render_offline(4_800);
        assert_eq!(audio.voice_state(world), Some(PlaybackState::Paused));
        let world_at = audio.voice_position(world).expect("world position");
        let ui_at = audio.voice_position(ui).expect("ui position");
        // Five seconds of mixer time and 300 frames of ticks while paused.
        for _ in 0..300 {
            audio.set_game_paused(true);
            audio.tick(1.0 / 60.0);
            let _ = audio.render_offline(800);
        }
        assert_eq!(
            audio.voice_position(world),
            Some(world_at),
            "the held world voice did not move"
        );
        let ui_now = audio.voice_position(ui).expect("ui position");
        assert!(
            ui_now - ui_at > 4.9,
            "the UI voice played on: {ui_at} -> {ui_now}"
        );
        assert_eq!(audio.held_voice_count(), 1, "only the world voice is held");

        audio.set_game_paused(false);
        let _ = audio.render_offline(4_800);
        let _ = audio.render_offline(4_800);
        assert_eq!(audio.voice_state(world), Some(PlaybackState::Playing));
        let resumed = audio.voice_position(world).expect("resumed");
        assert!(
            resumed > world_at && resumed - world_at < 0.2,
            "resumed in place: {world_at} -> {resumed}"
        );
        assert_eq!(audio.held_voice_count(), 0);
    }

    /// A voice the host paused itself stays paused after the gameplay pause ends; a voice that
    /// starts on a frozen bus during the pause is held the frame it starts.
    #[test]
    fn the_pause_resumes_only_what_it_held() {
        let mut audio = headless();
        let long = tone(48_000 * 4);
        let host = audio.start_asset(&long, on("World")).expect("host-paused voice");
        audio.pause_voice(host, Duration::ZERO);
        audio.set_game_paused(true);
        let late = audio
            .start_asset(&long, on("Ambience"))
            .expect("started while paused");
        audio.set_game_paused(true);
        let _ = audio.render_offline(4_800);
        assert_eq!(
            audio.voice_state(late),
            Some(PlaybackState::Paused),
            "held at start"
        );
        audio.set_game_paused(false);
        let _ = audio.render_offline(4_800);
        assert_eq!(
            audio.voice_state(host),
            Some(PlaybackState::Paused),
            "the host's pause stays"
        );
        assert_eq!(audio.voice_state(late), Some(PlaybackState::Playing));
    }

    /// Child buses follow their exempt parent; the exemption list is the game's to change.
    #[test]
    fn exempt_buses_cover_their_children_and_are_configurable() {
        let mut audio = headless();
        assert!(!audio.pause_freezes_bus("Ui"));
        assert!(!audio.pause_freezes_bus("Music"));
        assert!(audio.pause_freezes_bus("World"));
        assert!(audio.pause_freezes_bus("Sfx"));
        assert!(audio.pause_freezes_bus("Master"));
        audio.set_pause_exempt_buses(["Sfx"]);
        assert!(!audio.pause_freezes_bus("World"), "World is under Sfx");
        assert!(!audio.pause_freezes_bus("Ui"));
        assert!(audio.pause_freezes_bus("Music"));
    }

    /// While held, a virtualised voice's cursor stays where it was (no drift), and the revive
    /// re-applies the hold.
    #[test]
    fn a_held_virtual_voice_keeps_its_cursor_and_revives_held() {
        let mut audio = headless();
        let long = tone(48_000 * 6);
        let id = audio.start_asset(&long, on("World")).expect("voice");
        let _ = audio.render_offline(4_800);
        audio.set_game_paused(true);
        audio.voices.get_mut(id).expect("voice").virtualise();
        let cursor = audio.voices.get(id).expect("voice").virtual_cursor_secs;
        let age = audio.voices.get(id).expect("voice").age_secs;
        // The voice bookkeeping a tick runs (a full tick may also revive the voice, which resets
        // the cursor it hands over).
        for _ in 0..300 {
            audio.voices.tick_state(1.0 / 60.0);
        }
        let entry = audio.voices.get(id).expect("voice");
        assert_eq!(entry.virtual_cursor_secs, cursor, "the cursor did not drift");
        assert_eq!(entry.age_secs, age, "the age did not drift");
        audio.revive_voice(id);
        let _ = audio.render_offline(960);
        assert_eq!(audio.voice_state(id), Some(PlaybackState::Paused), "revived held");
        audio.set_game_paused(false);
        let _ = audio.render_offline(960);
        assert_eq!(audio.voice_state(id), Some(PlaybackState::Playing));
    }
}
