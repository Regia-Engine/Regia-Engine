// SPDX-License-Identifier: MPL-2.0
//! The music calls on the [`Audio`] facade (`docs/specs/16-audio.md` § 3a, ADR-2517). The logic is
//! in [`crate::music`]; this file gives it the buses and the clock.

use std::time::Duration;

use kira::clock::ClockSpeed;
use regia_ecs::prelude::*;

use super::{Audio, play_data_on};
use crate::error::AudioError;
use crate::instance::SoundInstance;
use crate::music::{
    MusicAsset, MusicBeat, MusicSchedule, MusicState, MusicSync, MusicTransitionRequest, MusicVoiceData,
    VoiceLink,
};
use crate::sound::{PlayData, PlaybackParams, SoundAsset, play_error};

/// One tick per second: the clock's `ticks + fraction` is the music time in seconds.
const CLOCK_SPEED: ClockSpeed = ClockSpeed::TicksPerSecond(1.0);

/// Starts one music voice on the Music bus, on the clock when it has a link.
fn start_voice(
    buses: &mut crate::bus::Buses,
    asset: &SoundAsset,
    params: &PlaybackParams,
    link: Option<VoiceLink>,
) -> Result<SoundInstance, AudioError> {
    let bus = "Music";
    let data = asset.play_data(params)?;
    let Some(link) = link else {
        // The fade-in half of an equal-power crossfade with the outgoing track's stems.
        let data = match params.fade_in {
            Some(fade) => data.with_fade_in(crate::sound::equal_power_fade_in(fade)),
            None => data,
        };
        return play_data_on(buses, bus, data);
    };
    let track = buses
        .track(bus)
        .ok_or_else(|| AudioError::UnknownBus(bus.into()))?;
    match data {
        PlayData::Static(inner) => Ok(SoundInstance::from_static(
            track.play(MusicVoiceData { inner, link }).map_err(play_error)?,
        )),
        PlayData::Streaming(inner) => Ok(SoundInstance::from_streaming(
            track.play(MusicVoiceData { inner, link }).map_err(play_error)?,
        )),
    }
}

impl Audio {
    /// The sample rate the music clock's frames count at: the output's, or a running mixdown's.
    pub fn music_sample_rate(&self) -> u32 {
        self.mixdown()
            .map_or(self.output_sample_rate, |job| job.sample_rate)
    }

    /// Crossfades to a music track over `fade`, at once (a fresh track fades the previous one
    /// out). [`Self::music_transition`] waits for a beat, a bar or the end of a segment instead.
    /// [`AudioError::Invalid`] when the track's bpm is not above 0 and at most 1000.
    pub fn play_music(&mut self, asset: &MusicAsset, fade: Duration) -> Result<(), AudioError> {
        // Before the clock exists: an unplayable tempo never reaches the mixer.
        crate::music::check_bpm(asset)?;
        let clock = self.backend.add_clock(CLOCK_SPEED);
        let buses = &mut self.buses;
        let mut start = |asset: &SoundAsset, params: &PlaybackParams, link: Option<VoiceLink>| {
            start_voice(buses, asset, params, link)
        };
        self.music.play(asset, fade, clock, &mut start)
    }

    /// Registers a track by name, for [`Self::music_transition`]; a track of the same name is
    /// replaced.
    pub fn register_music(&mut self, asset: MusicAsset) {
        self.music.register_track(asset);
    }

    /// Moves to the registered track `to` by the playing track's transition rule, with `request`
    /// over the rule. With no track playing it just starts, over the rule's crossfade if any.
    /// Asking for what is already playing or on its way changes nothing, so a game may call this
    /// every frame with the music state it wants.
    ///
    /// # Errors
    /// [`AudioError::UnknownEntry`] when `to` is not registered; [`AudioError::Invalid`] when its
    /// bpm is not above 0 and at most 1000; a play error from the mixer.
    pub fn music_transition(
        &mut self,
        to: &str,
        request: MusicTransitionRequest,
    ) -> Result<MusicSchedule, AudioError> {
        let sample_rate = self.music_sample_rate();
        if !self.music.can_schedule() {
            let asset = self
                .music
                .track(to)
                .cloned()
                .ok_or_else(|| AudioError::UnknownEntry(to.to_string()))?;
            let fade = request
                .crossfade_secs
                .filter(|secs| secs.is_finite() && *secs >= 0.0)
                .unwrap_or(0.0);
            self.play_music(&asset, Duration::from_secs_f64(fade))?;
            return Ok(MusicSchedule {
                name: to.to_string(),
                sync: MusicSync::Immediate,
                at_secs: 0.0,
                at_frame: 0,
                bar: 0,
                beat: 0,
                noop: false,
            });
        }
        let buses = &mut self.buses;
        let mut start = |asset: &SoundAsset, params: &PlaybackParams, link: Option<VoiceLink>| {
            start_voice(buses, asset, params, link)
        };
        self.music.transition(to, request, sample_rate, &mut start)
    }

    /// Plays the playing track's stinger `name` on its own sync, or on `sync` when given, starting
    /// exactly on that boundary.
    ///
    /// # Errors
    /// [`AudioError::Invalid`] when no track is playing, [`AudioError::UnknownEntry`] when the
    /// track has no such stinger, a play error from the mixer.
    pub fn music_stinger(
        &mut self,
        name: &str,
        sync: Option<MusicSync>,
    ) -> Result<MusicSchedule, AudioError> {
        let sample_rate = self.music_sample_rate();
        let buses = &mut self.buses;
        let mut start = |asset: &SoundAsset, params: &PlaybackParams, link: Option<VoiceLink>| {
            start_voice(buses, asset, params, link)
        };
        self.music.stinger(name, sync, sample_rate, &mut start)
    }

    /// Requests a music intensity change; applied at the next bar boundary.
    pub fn set_music_intensity(&mut self, intensity: f32) {
        self.music.set_intensity(intensity);
    }

    /// The music system's current stem level targets (test/inspection).
    pub fn music_stem_levels(&self) -> Vec<(String, f32)> {
        self.music.stem_levels()
    }

    /// The playing music track's name.
    pub fn current_music(&self) -> Option<&str> {
        self.music.current_track()
    }

    /// True while a music crossfade tail is running.
    pub fn music_crossfading(&self) -> bool {
        self.music.is_crossfading()
    }

    /// A snapshot of the music system: track, clock, position, the transition on its way.
    #[must_use]
    pub fn music_state(&self) -> MusicState {
        self.music.state()
    }

    /// The beats since the last call, oldest first. [`send_music_beats`] sends them as
    /// [`MusicBeat`] messages; a host without the plugin reads them here.
    pub fn take_music_beats(&mut self) -> Vec<MusicBeat> {
        self.music.take_beats()
    }

    /// Advances the music system by `dt` and applies a request that had to wait for its track.
    pub(super) fn tick_music(&mut self, dt: f64) {
        self.music.tick(dt);
        if let Some((to, request)) = self.music.take_queued_request()
            && let Err(error) = self.music_transition(&to, request)
        {
            tracing::warn!(target: "regia_audio", "the queued music transition to `{to}` failed: {error}");
        }
    }
}

/// `PostUpdate`/`AudioSync` system: sends the beats since the last frame as [`MusicBeat`]
/// messages, for gameplay to read in the next `Update`.
pub fn send_music_beats(mut audio: ResMut<Audio>, mut writer: MessageWriter<MusicBeat>) {
    for beat in audio.take_music_beats() {
        writer.write(beat);
    }
}
