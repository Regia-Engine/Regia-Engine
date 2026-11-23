// SPDX-License-Identifier: MPL-2.0
//! Time-stretched voices (ADR-2721): a decoded sound played at a pitch-preserving rate that may
//! change while it plays ([`crate::TimeStretchSoundData`]).

use std::sync::Arc;

use super::Audio;
use crate::{AudioError, SoundAsset, TimeStretchHandle, TimeStretchSoundData, check_rate};

/// The time-stretched voices of an [`Audio`].
#[derive(Default)]
pub(crate) struct StretchState {
    next_id: u64,
    voices: Vec<StretchVoice>,
}

struct StretchVoice {
    id: u64,
    label: String,
    bus: String,
    handle: TimeStretchHandle,
}

/// One time-stretched voice (`audio.time_stretch_probe`).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct TimeStretchVoiceInfo {
    /// The voice id.
    pub id: u64,
    /// The sound's label.
    pub label: String,
    /// The bus it plays on.
    pub bus: String,
    /// The rate now.
    pub rate: f32,
    /// The input position, seconds.
    pub position_secs: f64,
    /// WSOLA hops produced (0 while it has stayed at rate 1).
    pub hops: u64,
}

impl Audio {
    /// Plays `asset` on `bus` at `rate` (0.5..=2) without changing its pitch; returns the voice
    /// id for [`Audio::set_time_stretch_rate`] and [`Audio::stop_time_stretched`]. The sound is
    /// decoded into memory and brought to the output rate here. These voices are not counted by
    /// the voice limit or ducking (a handful of dialogue or music lines, not effects).
    ///
    /// # Errors
    /// [`AudioError::UnknownBus`]; [`AudioError::Invalid`] starting with the
    /// [`crate::TimeStretchError::code`] for a bad rate or an empty sound; a decode error.
    pub fn play_time_stretched(
        &mut self,
        asset: &SoundAsset,
        bus: &str,
        rate: f32,
        looping: bool,
        volume_db: f32,
    ) -> Result<u64, AudioError> {
        let rate = check_rate(rate).map_err(|e| AudioError::Invalid(format!("{}: {e}", e.code())))?;
        if !self.buses.contains(bus) {
            return Err(AudioError::UnknownBus(bus.to_string()));
        }
        let decoded = asset.decode_to_memory()?;
        let (frames, source_rate) = decoded
            .static_frames()
            .ok_or_else(|| AudioError::load(asset.label(), "no decoded frames"))?;
        let output_rate = self
            .mixdown()
            .map_or(self.output_sample_rate, |job| job.sample_rate);
        let frames: Arc<[kira::Frame]> = if source_rate == output_rate {
            Arc::from(frames)
        } else {
            let left: Vec<f32> = frames.iter().map(|f| f.left).collect();
            let right: Vec<f32> = frames.iter().map(|f| f.right).collect();
            let left = crate::resample_sinc(&left, source_rate, output_rate);
            let right = crate::resample_sinc(&right, source_rate, output_rate);
            left.iter()
                .zip(&right)
                .map(|(l, r)| kira::Frame { left: *l, right: *r })
                .collect()
        };
        let gain = crate::spatial::db_to_gain(crate::bus::finite_db(volume_db + asset.loudness_gain_db()));
        let data = TimeStretchSoundData::new(frames, output_rate, rate, looping, gain)
            .map_err(|e| AudioError::Invalid(format!("{}: {e}", e.code())))?;
        let track = self
            .buses
            .track(bus)
            .ok_or_else(|| AudioError::UnknownBus(bus.to_string()))?;
        let handle = track
            .play(data)
            .map_err(|e| AudioError::PlayFailed(format!("{e:?}")))?;
        self.stretch.next_id += 1;
        let id = self.stretch.next_id;
        self.stretch.voices.push(StretchVoice {
            id,
            label: asset.label().to_string(),
            bus: bus.to_string(),
            handle,
        });
        Ok(id)
    }

    /// Changes a time-stretched voice's rate (0.5..=2); the next WSOLA hop uses it.
    ///
    /// # Errors
    /// [`AudioError::UnknownEntry`] for an unknown or finished voice; [`AudioError::Invalid`] for
    /// a bad rate (`stretch_rate`).
    pub fn set_time_stretch_rate(&mut self, id: u64, rate: f32) -> Result<(), AudioError> {
        let voice = self
            .stretch
            .voices
            .iter()
            .find(|v| v.id == id)
            .ok_or_else(|| AudioError::UnknownEntry(format!("time-stretch voice {id}")))?;
        voice
            .handle
            .set_rate(rate)
            .map_err(|e| AudioError::Invalid(format!("{}: {e}", e.code())))
    }

    /// Stops a time-stretched voice with a short declick fade. False for an unknown voice.
    pub fn stop_time_stretched(&mut self, id: u64) -> bool {
        match self.stretch.voices.iter().find(|v| v.id == id) {
            Some(voice) => {
                voice.handle.stop();
                true
            }
            None => false,
        }
    }

    /// The playing time-stretched voices (finished ones are dropped by [`Audio::tick`]).
    pub fn time_stretch_voices(&self) -> Vec<TimeStretchVoiceInfo> {
        self.stretch
            .voices
            .iter()
            .map(|v| TimeStretchVoiceInfo {
                id: v.id,
                label: v.label.clone(),
                bus: v.bus.clone(),
                rate: v.handle.rate(),
                position_secs: v.handle.position_secs(),
                hops: v.handle.hops(),
            })
            .collect()
    }

    /// Drops finished time-stretched voices (once a frame).
    pub(crate) fn tick_stretch(&mut self) {
        self.stretch.voices.retain(|v| !v.handle.finished());
    }

    /// The bus rebuild ended every time-stretched voice.
    pub(crate) fn reset_stretch_after_rebuild(&mut self) {
        self.stretch.voices.clear();
    }
}
