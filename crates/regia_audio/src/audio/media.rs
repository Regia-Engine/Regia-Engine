// SPDX-License-Identifier: MPL-2.0
//! `Audio::play_media_feed`: a video player's PCM feed on a bus (specs/50 § 3.3, ADR-1670).

use super::Audio;
use crate::error::AudioError;
use crate::media_feed::{MediaStreamData, MediaStreamHandle, PcmConsumer};

impl Audio {
    /// The bus `bus` names (matched without case), else `Master` with a warning.
    fn media_bus(&self, bus: &str) -> Result<String, AudioError> {
        if let Some(found) = self.buses.names().find(|n| n.eq_ignore_ascii_case(bus)) {
            return Ok(found.to_string());
        }
        tracing::warn!(target: "regia_audio", bus, "audio_bus_missing: media feed plays on Master");
        self.buses
            .names()
            .find(|n| n.eq_ignore_ascii_case("master"))
            .map(str::to_string)
            .ok_or_else(|| AudioError::UnknownBus(bus.to_string()))
    }

    /// Plays `data` on the bus `name` (already resolved).
    fn play_media_data(&mut self, name: &str, data: MediaStreamData) -> Result<(), AudioError> {
        let track = self
            .buses
            .track(name)
            .ok_or_else(|| AudioError::UnknownBus(name.to_string()))?;
        track.play(data).map_err(crate::sound::play_error)?;
        Ok(())
    }

    /// Restarts a media stream on the rebuilt mixer after an output suspend: `handle`'s consumer,
    /// volume, frame count and stop flag carry over. Returns the bus.
    pub(crate) fn resume_media_feed(
        &mut self,
        bus: &str,
        sample_rate: u32,
        handle: &MediaStreamHandle,
    ) -> Result<String, AudioError> {
        let name = self.media_bus(bus)?;
        let data = MediaStreamData::resumed(sample_rate, self.output_sample_rate, handle);
        self.play_media_data(&name, data)?;
        Ok(name)
    }
}

impl Audio {
    /// Plays `consumer` (at `sample_rate` Hz) on `bus`, matched without case (`sfx` finds `Sfx`). A
    /// missing bus falls back to `Master` with a warning (`audio_bus_missing`). Returns the stream
    /// handle and the bus used.
    pub fn play_media_feed(
        &mut self,
        bus: &str,
        consumer: PcmConsumer,
        sample_rate: u32,
        volume: f32,
    ) -> Result<(MediaStreamHandle, String), AudioError> {
        let name = self.media_bus(bus)?;
        let data = MediaStreamData::new(consumer, sample_rate, self.output_sample_rate, volume);
        let handle = data.handle();
        self.play_media_data(&name, data)?;
        // Exact bus name: the default duck rule's `when` is `"Voice"`, not a case-folded alias.
        // `resume_media_feed` reuses this handle and does not call here, so a resume is not a
        // second count.
        if name == "Voice" {
            self.voice_bus_feeds.push(handle.clone());
        }
        Ok((handle, name))
    }
}
