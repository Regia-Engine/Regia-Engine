// SPDX-License-Identifier: MPL-2.0
//! Streaming underrun watch on the [`Audio`] facade (ADR-872).
//!
//! kira decodes a streaming sound on a background thread into a short ring buffer; when that
//! buffer runs dry the sound is held silent, without advancing, until the decoder catches up, so
//! it plays late from then on. Nothing reports it. Once per tick the watch compares each playing
//! stream's position with the previous one: a stream that had been advancing and stayed put for
//! [`STARVE_SECS`] of rendered audio is starved, which is counted ([`Audio::stream_stats`]) and
//! logged. Rendered audio is the frame delta on the device (`dt`) and the frames actually
//! rendered on the offline backend, so a headless run that does not render never counts one.

use std::collections::{HashMap, HashSet};

use kira::sound::PlaybackState;

use super::Audio;
use crate::instance::SoundInstance;
use crate::sound::StreamStats;
use crate::voices::SoundInstanceId;

/// Frozen this long (seconds of rendered audio) while playing = starved. A device delivers
/// 5–20 ms blocks, so a tick without a new block is normal; 100 ms without one is not.
const STARVE_SECS: f64 = 0.1;

/// A position this close to the end of a stream that does not loop is its natural stop.
const END_GUARD_SECS: f64 = 0.1;

/// Underruns after which further ones are logged at debug level only.
const WARN_LIMIT: u64 = 8;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum StreamKey {
    Voice(SoundInstanceId),
    Stem(String),
}

#[derive(Clone, Copy, Debug)]
struct Watch {
    last_position: f64,
    advanced: bool,
    frozen_secs: f64,
    starved: bool,
}

/// One stream as seen this tick.
struct Sighting<'a> {
    key: StreamKey,
    label: &'a str,
    playing: bool,
    position: f64,
    /// Where a stream that does not loop ends, seconds.
    end_secs: Option<f64>,
}

/// The per-stream watch state and the counters.
pub(super) struct StreamWatch {
    sample_rate: u32,
    rendered_frames: u64,
    watches: HashMap<StreamKey, Watch>,
    stats: StreamStats,
}

impl StreamWatch {
    pub(super) fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            rendered_frames: 0,
            watches: HashMap::new(),
            stats: StreamStats::default(),
        }
    }

    /// Sets the rate offline frames are counted at (a mixdown runs its own rate, ADR-1520).
    pub(super) fn set_sample_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate.max(1);
    }

    /// Counts frames the offline backend rendered (the watch's clock there).
    pub(super) fn note_rendered(&mut self, frames: usize) {
        self.rendered_frames = self.rendered_frames.saturating_add(frames as u64);
    }

    /// Takes the offline rendered time since the last call, seconds.
    fn take_rendered_secs(&mut self) -> f64 {
        std::mem::take(&mut self.rendered_frames) as f64 / f64::from(self.sample_rate)
    }

    /// Updates one stream; returns whether it is starved now.
    fn observe(&mut self, sighting: &Sighting<'_>, elapsed: f64) -> bool {
        let watch = self.watches.entry(sighting.key.clone()).or_insert(Watch {
            last_position: sighting.position,
            advanced: false,
            frozen_secs: 0.0,
            starved: false,
        });
        if !sighting.playing || sighting.position != watch.last_position {
            watch.advanced |= sighting.playing && sighting.position != watch.last_position;
            watch.last_position = sighting.position;
            watch.frozen_secs = 0.0;
            watch.starved = false;
            return false;
        }
        let at_end = sighting
            .end_secs
            .is_some_and(|end| sighting.position >= end - END_GUARD_SECS);
        if !watch.advanced || at_end || elapsed <= 0.0 {
            return false;
        }
        watch.frozen_secs += elapsed;
        if watch.frozen_secs < STARVE_SECS {
            return false;
        }
        if watch.starved {
            self.stats.starved_secs += elapsed;
        } else {
            watch.starved = true;
            self.stats.underruns += 1;
            self.stats.starved_secs += watch.frozen_secs;
            if self.stats.underruns <= WARN_LIMIT {
                tracing::warn!(
                    target: "regia_audio",
                    "stream `{}` ran dry at {:.2} s: its decoder is behind (slow storage or a loaded \
                     machine), so it plays late from here (underrun {})",
                    sighting.label,
                    sighting.position,
                    self.stats.underruns
                );
            } else {
                tracing::debug!(target: "regia_audio", "stream `{}` ran dry", sighting.label);
            }
        }
        true
    }
}

/// A sighting of a kira instance.
fn sighting<'a>(
    key: StreamKey,
    label: &'a str,
    instance: &SoundInstance,
    end_secs: Option<f64>,
) -> Sighting<'a> {
    Sighting {
        key,
        label,
        playing: instance.state() == PlaybackState::Playing,
        position: instance.position(),
        end_secs,
    }
}

impl Audio {
    /// Underrun counters of the streaming voices and music stems (ADR-872): how many play now,
    /// how many are starved, how often and how long streams ran dry since the start.
    pub fn stream_stats(&self) -> StreamStats {
        self.streams.stats
    }

    /// Counts offline-rendered frames for the watch (called by [`Audio::render_offline`]).
    pub(super) fn note_rendered_frames(&mut self, frames: usize) {
        self.streams.note_rendered(frames);
    }

    /// One watch step (called by [`Audio::tick`] with the frame delta).
    pub(super) fn watch_streams(&mut self, dt: f64) {
        let _span = tracing::trace_span!("audio_watch_streams").entered();
        let elapsed = if self.backend.is_offline() {
            self.streams.take_rendered_secs()
        } else {
            dt
        };
        if self.suspended {
            // The output is gone: nothing plays, nothing can starve.
            self.streams.watches.clear();
            self.streams.stats.streaming_voices = 0;
            self.streams.stats.starved_voices = 0;
            return;
        }
        let stems: Vec<(String, &SoundInstance)> = self.music.current_stems().collect();
        let mut sightings = Vec::new();
        for entry in self.voices.iter() {
            let Some(instance) = entry.instance.as_ref() else {
                continue;
            };
            if !entry.asset.is_streaming() {
                continue;
            }
            let duration = entry.asset.duration_secs();
            let end_secs = (entry.params.loop_region.is_none() && duration > 0.0).then_some(duration);
            let mut seen = sighting(
                StreamKey::Voice(entry.id),
                entry.asset.label(),
                instance,
                end_secs,
            );
            // A voice held at zero speed is not starved.
            seen.playing &= entry.params.pitch != 0.0;
            sightings.push(seen);
        }
        for (name, instance) in &stems {
            sightings.push(sighting(StreamKey::Stem(name.clone()), name, instance, None));
        }
        let mut starved = 0;
        let mut seen = HashSet::new();
        for s in &sightings {
            if self.streams.observe(s, elapsed) {
                starved += 1;
            }
            seen.insert(s.key.clone());
        }
        self.streams.watches.retain(|key, _| seen.contains(key));
        self.streams.stats.streaming_voices = sightings.len();
        self.streams.stats.starved_voices = starved;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::{Audio, Sighting, StreamKey, StreamWatch, Watch};
    use crate::music::{MusicAsset, MusicStem};
    use crate::settings::AudioSettings;
    use crate::sound::{PlaybackParams, SoundAsset, StreamStats};
    use crate::voices::{SoundInstanceId, VoiceEntry, VoiceKind, voice_priority};

    fn headless() -> Audio {
        Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        })
    }

    fn voice(id: u64, playing: bool, position: f64, end_secs: Option<f64>) -> Sighting<'static> {
        Sighting {
            key: StreamKey::Voice(SoundInstanceId(id)),
            label: "voice",
            playing,
            position,
            end_secs,
        }
    }

    fn arm(watch: &mut StreamWatch, id: u64) {
        let before = watch.stats;
        assert!(!watch.observe(&voice(id, true, 0.0, None), 1.0));
        assert!(!watch.observe(&voice(id, true, 0.25, None), 1.0));
        assert_eq!(watch.stats, before, "arming a stream does not count an underrun");
    }

    fn silence_wav(frames: usize) -> Arc<Vec<u8>> {
        let data_len = frames.checked_mul(2).expect("frame count fits");
        let riff_len = u32::try_from(36 + data_len).expect("riff size");
        let data_len_u = u32::try_from(data_len).expect("data size");
        let mut out = Vec::with_capacity(44 + data_len);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_len.to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&48_000u32.to_le_bytes());
        out.extend_from_slice(&(48_000u32 * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len_u.to_le_bytes());
        out.resize(out.len() + data_len, 0);
        Arc::new(out)
    }

    fn silence_stream() -> SoundAsset {
        SoundAsset::streaming_from_bytes(silence_wav(4_800), "silence.wav").expect("wav parses")
    }

    fn stem(name: &str) -> MusicStem {
        MusicStem {
            name: name.to_string(),
            asset: crate::test_support::asset(),
            base_db: 0.0,
            intensity: Vec::new(),
        }
    }

    #[test]
    fn rendered_time_follows_the_sample_rate_and_saturates() {
        let mut watch = StreamWatch::new(0);
        assert_eq!(watch.sample_rate, 1, "a zero rate cannot divide the frame clock");
        watch.note_rendered(0);
        assert_eq!(watch.take_rendered_secs(), 0.0);
        watch.note_rendered(4);
        assert_eq!(watch.take_rendered_secs(), 4.0);
        assert_eq!(watch.take_rendered_secs(), 0.0, "taking the clock clears it");

        let mut watch = StreamWatch::new(48_000);
        watch.note_rendered(24_000);
        watch.note_rendered(24_000);
        assert_eq!(watch.take_rendered_secs(), 1.0);

        watch.rendered_frames = u64::MAX - 3;
        watch.note_rendered(10);
        assert_eq!(watch.rendered_frames, u64::MAX);
        watch.note_rendered(1);
        assert_eq!(watch.rendered_frames, u64::MAX);
    }

    #[test]
    fn a_stream_that_has_not_moved_yet_is_not_starved() {
        let mut watch = StreamWatch::new(48_000);
        assert!(!watch.observe(&voice(1, true, 0.0, None), 1.0));
        assert!(!watch.observe(&voice(1, true, 0.0, None), 5.0));
        assert_eq!(watch.stats, StreamStats::default());
        let stored = watch
            .watches
            .get(&StreamKey::Voice(SoundInstanceId(1)))
            .expect("watched");
        assert!(!stored.advanced);
        assert_eq!(stored.frozen_secs, 0.0);
        assert!(!stored.starved);
    }

    #[test]
    fn starvation_begins_at_one_tenth_second_and_keeps_counting() {
        let mut watch = StreamWatch::new(48_000);
        arm(&mut watch, 1);

        assert!(!watch.observe(&voice(1, true, 0.25, None), 0.0625));
        assert_eq!(watch.stats.underruns, 0);
        assert_eq!(watch.stats.starved_secs, 0.0);

        // 0.0625 + 0.0625 = 0.125, the first step that is not below the 0.1 s line.
        // The whole freeze is booked, including the part before the line.
        assert!(watch.observe(&voice(1, true, 0.25, None), 0.0625));
        assert_eq!(watch.stats.underruns, 1);
        assert_eq!(watch.stats.starved_secs, 0.125);

        assert!(watch.observe(&voice(1, true, 0.25, None), 0.5));
        assert_eq!(watch.stats.underruns, 1, "one stuck stretch is one underrun");
        assert_eq!(watch.stats.starved_secs, 0.625);
    }

    #[test]
    fn playback_changes_clear_a_freeze() {
        let mut watch = StreamWatch::new(48_000);
        arm(&mut watch, 1);
        assert!(!watch.observe(&voice(1, true, 0.25, None), 0.0));
        assert!(!watch.observe(&voice(1, true, 0.25, None), -1.0));
        assert!(!watch.observe(&voice(1, true, 0.25, None), 0.0625));
        assert_eq!(
            watch.stats,
            StreamStats::default(),
            "zero or negative steps do not accumulate"
        );

        assert!(!watch.observe(&voice(1, false, 0.25, None), 1.0));
        assert!(!watch.observe(&voice(1, true, 0.25, None), 0.0625));
        assert_eq!(watch.stats.underruns, 0, "silence clears the partial freeze");
        assert!(watch.observe(&voice(1, true, 0.25, None), 0.0625));
        assert_eq!(watch.stats.underruns, 1);
        assert_eq!(watch.stats.starved_secs, 0.125);

        assert!(!watch.observe(&voice(1, true, 0.5, None), 1.0));
        assert!(watch.observe(&voice(1, true, 0.5, None), 0.1));
        assert_eq!(watch.stats.underruns, 2);
        assert_eq!(watch.stats.starved_secs, 0.125 + 0.1);
    }

    #[test]
    fn the_natural_end_of_a_one_shot_is_not_an_underrun() {
        let mut watch = StreamWatch::new(48_000);
        arm(&mut watch, 1);
        let end = 1.0;
        let guard = end - 0.1;
        assert!(!watch.observe(&voice(1, true, guard, Some(end)), 1.0));
        assert!(!watch.observe(&voice(1, true, guard, Some(end)), 1.0));
        assert!(!watch.observe(&voice(1, true, end + 1.0, Some(end)), 1.0));
        assert!(!watch.observe(&voice(1, true, end + 1.0, Some(end)), 1.0));
        assert_eq!(watch.stats, StreamStats::default());

        // The same playhead with no known end (a loop, or a stem) does starve.
        assert!(!watch.observe(&voice(1, true, guard, None), 1.0));
        assert!(watch.observe(&voice(1, true, guard, None), 0.125));
        assert_eq!(watch.stats.underruns, 1);
        assert_eq!(watch.stats.starved_secs, 0.125);

        // A clip shorter than the guard is entirely "at the end".
        assert!(!watch.observe(&voice(2, true, 0.0, Some(0.05)), 0.0));
        assert!(!watch.observe(&voice(2, true, 0.01, Some(0.05)), 0.0));
        assert!(!watch.observe(&voice(2, true, 0.01, Some(0.05)), 1.0));
        assert_eq!(watch.stats.underruns, 1);

        // Away from the end, the same stream starves on a held position.
        assert!(!watch.observe(&voice(1, true, 0.25, Some(end)), 1.0));
        assert!(watch.observe(&voice(1, true, 0.25, Some(end)), 0.125));
        assert_eq!(watch.stats.underruns, 2);
        assert_eq!(watch.stats.starved_secs, 0.25);
    }

    #[test]
    fn a_nan_position_ends_the_freeze_and_the_next_hold_counts_again() {
        let mut watch = StreamWatch::new(48_000);
        arm(&mut watch, 1);
        assert!(watch.observe(&voice(1, true, 0.25, None), 0.125));
        assert!(!watch.observe(&voice(1, true, f64::NAN, None), 1.0));
        assert!(!watch.observe(&voice(1, true, f64::NAN, None), 1.0));
        assert_eq!(watch.stats.underruns, 1);
        // The NaN compares unequal to the old playhead, so this step only re-arms.
        assert!(!watch.observe(&voice(1, true, 0.25, None), 0.125));
        assert!(watch.observe(&voice(1, true, 0.25, None), 0.125));
        assert_eq!((watch.stats.underruns, watch.stats.starved_secs), (2, 0.25));
    }

    #[test]
    fn separate_streams_count_separate_underruns_past_the_log_limit() {
        let mut watch = StreamWatch::new(48_000);
        for id in 1..=9 {
            arm(&mut watch, id);
            assert!(watch.observe(&voice(id, true, 0.25, None), 0.125));
        }
        assert_eq!(watch.stats.underruns, 9);
        assert_eq!(watch.stats.starved_secs, 1.125);

        // A tenth key that only reached the partial freeze stays out of the counters.
        arm(&mut watch, 10);
        assert!(!watch.observe(&voice(10, true, 0.25, None), 0.0625));
        assert_eq!(watch.stats.underruns, 9);
        assert_eq!(watch.stats.starved_secs, 1.125);

        // A stem is its own key: it does not share freeze time with voice 1.
        let stem_at = |position: f64| Sighting {
            key: StreamKey::Stem("level/drums".to_string()),
            label: "level/drums",
            playing: true,
            position,
            end_secs: None,
        };
        assert!(!watch.observe(&stem_at(0.0), 1.0));
        assert!(!watch.observe(&stem_at(1.5), 1.0));
        assert!(watch.observe(&stem_at(1.5), 0.125));
        assert_eq!(watch.stats.underruns, 10);
        assert_eq!(watch.stats.starved_secs, 1.25);
    }

    #[test]
    fn offline_watch_uses_rendered_frames_not_the_frame_delta() {
        let mut audio = headless();
        let track = MusicAsset::from_stems("level", 120.0, 4, vec![stem("a"), stem("b")]);
        audio.play_music(&track, Duration::ZERO).expect("music starts");
        audio
            .start_asset(&crate::test_support::asset(), PlaybackParams::default())
            .expect("static one-shot");
        let stream_id = audio
            .start_asset(&silence_stream(), PlaybackParams::default())
            .expect("stream starts");
        audio
            .voices
            .get_mut(stream_id)
            .expect("stream voice")
            .params
            .pitch = 0.0;
        let virtual_id = audio.voices.push(VoiceEntry {
            id: SoundInstanceId(0),
            kind: VoiceKind::OneShot,
            bus: "Master".into(),
            cue: None,
            asset: SoundAsset::from_streaming_factory(
                || Err(crate::error::AudioError::Invalid("unplayed".into())),
                3.0,
                "virtual",
            ),
            params: PlaybackParams::default(),
            base_volume_db: 0.0,
            gain: 1.0,
            priority: voice_priority::NORMAL,
            instance: None,
            filter_track: None,
            virtual_cursor_secs: 0.0,
            age_secs: 0.0,
            stopping: false,
            paused: false,
            held: false,
        });

        // No frames rendered: a large `dt` is not the offline clock, and nothing has moved.
        audio.watch_streams(100.0);
        assert_eq!(
            audio.stream_stats(),
            StreamStats {
                streaming_voices: 3,
                starved_voices: 0,
                underruns: 0,
                starved_secs: 0.0,
            }
        );
        assert!(
            audio
                .streams
                .watches
                .contains_key(&StreamKey::Stem("level/a".into()))
        );
        assert!(
            audio
                .streams
                .watches
                .contains_key(&StreamKey::Stem("level/b".into()))
        );
        assert!(audio.streams.watches.contains_key(&StreamKey::Voice(stream_id)));
        assert!(!audio.streams.watches.contains_key(&StreamKey::Voice(virtual_id)));

        // Pretend the stems have already advanced, then hold them while half a second renders.
        for watch in audio.streams.watches.values_mut() {
            watch.advanced = true;
        }
        audio.streams.watches.insert(
            StreamKey::Voice(SoundInstanceId(u64::MAX)),
            Watch {
                last_position: 0.0,
                advanced: true,
                frozen_secs: 0.25,
                starved: false,
            },
        );
        audio.note_rendered_frames(24_000);
        audio.watch_streams(100.0);
        assert_eq!(
            audio.streams.rendered_frames, 0,
            "the watch consumed the render clock"
        );
        assert!(
            !audio
                .streams
                .watches
                .contains_key(&StreamKey::Voice(SoundInstanceId(u64::MAX)))
        );
        assert_eq!(
            audio.stream_stats(),
            StreamStats {
                streaming_voices: 3,
                starved_voices: 2,
                underruns: 2,
                starved_secs: 1.0,
            }
        );

        // Still held: the next half second extends the same underruns, it does not add new ones.
        audio.note_rendered_frames(24_000);
        audio.watch_streams(0.0);
        assert_eq!(
            audio.stream_stats(),
            StreamStats {
                streaming_voices: 3,
                starved_voices: 2,
                underruns: 2,
                starved_secs: 2.0,
            }
        );
    }

    #[test]
    fn suspended_output_keeps_underrun_history_and_drops_the_live_watch() {
        let mut audio = headless();
        audio.streams.stats = StreamStats {
            streaming_voices: 4,
            starved_voices: 2,
            underruns: 6,
            starved_secs: 1.5,
        };
        audio.streams.watches.insert(
            StreamKey::Stem("old".into()),
            Watch {
                last_position: 1.0,
                advanced: true,
                frozen_secs: 0.5,
                starved: true,
            },
        );
        audio.note_rendered_frames(48_000);
        audio.suspend_output();
        audio.watch_streams(4.0);
        assert!(audio.streams.watches.is_empty());
        assert_eq!(audio.streams.rendered_frames, 0);
        assert_eq!(
            audio.stream_stats(),
            StreamStats {
                streaming_voices: 0,
                starved_voices: 0,
                underruns: 6,
                starved_secs: 1.5,
            }
        );

        assert!(audio.resume_output());
        audio.note_rendered_frames(48_000);
        audio.watch_streams(9.0);
        let stats = audio.stream_stats();
        assert_eq!(
            (stats.underruns, stats.starved_secs, stats.streaming_voices),
            (6, 1.5, 0)
        );
        assert_eq!(audio.streams.rendered_frames, 0);
    }
}
