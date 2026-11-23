// SPDX-License-Identifier: MPL-2.0
//! Playing decoded speakers on the `Voice` bus: the chat emitter of a proximity channel and the
//! flat path of every other channel (`docs/specs/59-voice-chat.md` §§ 2.12, 2.14).
//!
//! Placement uses the engine's spatial computation ([`crate::spatial::spatialize`]) and its
//! occlusion answers. Rendering happens on the game thread, one 20 ms frame at a time, into a stereo
//! PCM feed on the `Voice` bus ([`crate::media_feed`]): the gains ramp across each frame, so a moving
//! speaker has no zipper noise and the output of a fixed set of frames is reproducible
//! (spec § 7). ADR-1911 explains why this is not a kira voice on an emitter track.

use super::session::{VoiceChatResource, VoiceChatSession};
use super::speaker::SpeakerState;
use crate::audio::Audio;
use crate::media_feed::{MediaStreamHandle, PcmFeed, PcmProducer};
use crate::occlusion::{AudioOcclusion, OcclusionRay};
use crate::spatial::{
    AudioEmitter, EmitterFrame, ListenerFrame, RolloffCurve, db_to_gain, nearest_listener, spatialize,
};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_scene::GlobalTransform;
use std::collections::BTreeMap;

/// Level of a speaker on a spatial channel whose entity is not resolved yet (spec § 2.12).
pub const FLAT_UNRESOLVED_DB: f32 = -6.0;
/// Frames of the crossfade when a speaker changes between flat and spatial (100 ms).
pub const SPATIAL_CROSSFADE_FRAMES: u32 = 5;
/// Frames of output kept ahead of the mixer. Starts here so one late game frame does not underrun.
const PREBUFFER_FRAMES: usize = 2;
/// Frames of output queued before the oldest frame of a burst is dropped (absorbs clock drift
/// between the game and the output device).
const MAX_QUEUED_FRAMES: usize = 6;
/// Frames without audio before a speaker's output stream is released (1 s).
const STREAM_IDLE_FRAMES: u32 = 50;
/// Occlusion smoothing step per second is the same as a sound emitter's; this is the voice frame.
const FRAME_SECONDS: f32 = 0.02;

/// Places a remote speaker in the world. Put it on the character (or a mouth child) of the player
/// that holds session slot `sender`; the speaker's voice then comes from there on spatial channels.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct VoiceChatEmitter {
    /// The speaker's session slot.
    pub sender: u16,
    /// Full volume inside this distance, metres (spec default 2 m).
    pub min_distance: f32,
    /// Silence past this distance, metres; `None` uses the channel's radius.
    pub max_distance: Option<f32>,
    /// Decibels per doubling of distance past `min_distance` (logarithmic rolloff).
    pub db_per_doubling: f32,
    /// Geometry between the speaker and the listener muffles the voice (needs an occlusion
    /// provider; see [`AudioOcclusion`]).
    pub occlusion: bool,
    /// Air absorption: farther voices lose their highs.
    pub air_absorption: bool,
}

impl Default for VoiceChatEmitter {
    fn default() -> Self {
        Self {
            sender: 0,
            min_distance: 2.0,
            max_distance: None,
            db_per_doubling: 6.0,
            occlusion: true,
            air_absorption: false,
        }
    }
}

/// On a [`VoiceChatEmitter`] entity while its speaker is talking (spec § 2.14), so a HUD or a mouth
/// animation never depends on hearing the audio.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct VoiceChatSpeaking {
    /// Level 0..1 (linear RMS of the last frame).
    pub level: f32,
}

/// The [`AudioEmitter`] parameters of a chat emitter on a channel of `radius` metres: the attenuation
/// of spec § 2.12, no cone, no Doppler.
#[must_use]
pub fn voice_emitter_params(emitter: &VoiceChatEmitter, radius: f32) -> AudioEmitter {
    AudioEmitter {
        autoplay: false,
        looping: false,
        min_distance: emitter.min_distance,
        max_distance: emitter.max_distance.unwrap_or(radius),
        rolloff: RolloffCurve::Log {
            db_per_doubling: emitter.db_per_doubling,
        },
        doppler_factor: 0.0,
        occlusion: emitter.occlusion,
        air_absorption: emitter.air_absorption,
        bus: Some("Voice".to_string()),
        ..AudioEmitter::default()
    }
}

/// Where one speaker's frame goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VoicePlacement {
    /// A channel that is not spatial: flat and centered.
    Center,
    /// A spatial channel with no entity yet: flat at [`FLAT_UNRESOLVED_DB`].
    Unresolved,
    /// In the world.
    Spatial {
        /// Distance, cone and occlusion gain, 0..1.
        gain: f32,
        /// Pan −1..1.
        pan: f32,
        /// Low-pass cutoff in Hz (air absorption and occlusion), if any.
        cutoff_hz: Option<f64>,
    },
}

/// A spatial placement of one speaker against one listener. `occlusion_gain` and `occlusion_cutoff`
/// are the smoothed occlusion answers (1 and `None` when clear).
#[must_use]
pub fn voice_placement(
    listener: &ListenerFrame,
    frame: &EmitterFrame,
    params: &AudioEmitter,
    occlusion_gain: f32,
    occlusion_cutoff: Option<f64>,
) -> VoicePlacement {
    let spatial = spatialize(listener, frame, params);
    let cutoff_hz = match (spatial.cutoff_hz, occlusion_cutoff) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    VoicePlacement::Spatial {
        gain: spatial.gain * occlusion_gain,
        pan: spatial.pan,
        cutoff_hz,
    }
}

/// `(left, right)` gains of a pan in −1..1: the sqrt law times √2 (center is 1, 1), as kira's
/// `Frame::panned`. Shared by the voice-chat speaker mixer and the loopback stream's
/// `placement_target`, so the two stay one law.
pub(crate) fn pan_gains(gain: f32, pan: f32) -> [f32; 2] {
    let x = (pan.clamp(-1.0, 1.0) + 1.0) * 0.5;
    let root_two = std::f32::consts::SQRT_2;
    [gain * (1.0 - x).sqrt() * root_two, gain * x.sqrt() * root_two]
}

/// Renders one speaker's mono frames to stereo. Pure: no device and no clock, so tests read the
/// exact samples.
#[derive(Clone, Debug)]
pub struct SpeakerMixer {
    rate: f32,
    gains: [f32; 2],
    kind: u8,
    fade_left: u32,
    low_pass: f32,
    started: bool,
}

impl SpeakerMixer {
    /// A mixer for a stream at `rate` Hz.
    #[must_use]
    pub fn new(rate: u32) -> Self {
        Self {
            rate: rate.max(1) as f32,
            gains: [0.0; 2],
            kind: 0,
            fade_left: 0,
            low_pass: 0.0,
            started: false,
        }
    }

    fn target(placement: VoicePlacement) -> ([f32; 2], u8, Option<f64>) {
        match placement {
            VoicePlacement::Center => ([1.0, 1.0], 1, None),
            VoicePlacement::Unresolved => ([db_to_gain(FLAT_UNRESOLVED_DB); 2], 2, None),
            VoicePlacement::Spatial { gain, pan, cutoff_hz } => (pan_gains(gain, pan), 3, cutoff_hz),
        }
    }

    /// Appends the stereo rendering of `mono` to `out`. The gains ramp over the frame; a change
    /// between flat and spatial takes [`SPATIAL_CROSSFADE_FRAMES`] frames.
    pub fn render(&mut self, mono: &[f32], placement: VoicePlacement, out: &mut Vec<f32>) {
        let (target, kind, cutoff_hz) = Self::target(placement);
        if !self.started {
            self.started = true;
            self.gains = target;
        } else if kind != self.kind {
            self.fade_left = SPATIAL_CROSSFADE_FRAMES;
        }
        self.kind = kind;
        let frames = self.fade_left.max(1) as f32;
        self.fade_left = self.fade_left.saturating_sub(1);
        let n = mono.len().max(1) as f32;
        let step = [
            (target[0] - self.gains[0]) / (frames * n),
            (target[1] - self.gains[1]) / (frames * n),
        ];
        // One-pole low-pass: a = 1 - e^(-2 pi fc / rate).
        let alpha = cutoff_hz
            .filter(|fc| *fc < f64::from(self.rate) * 0.5)
            .map(|fc| 1.0 - (-std::f32::consts::TAU * fc as f32 / self.rate).exp());
        out.reserve(mono.len() * 2);
        for s in mono {
            let dry = if s.is_finite() { *s } else { 0.0 };
            let wet = match alpha {
                Some(a) => {
                    self.low_pass += a * (dry - self.low_pass);
                    self.low_pass
                }
                None => {
                    self.low_pass = dry;
                    dry
                }
            };
            self.gains[0] += step[0];
            self.gains[1] += step[1];
            out.push((wet * self.gains[0]).clamp(-1.0, 1.0));
            out.push((wet * self.gains[1]).clamp(-1.0, 1.0));
        }
    }

    /// The current `(left, right)` gains.
    #[must_use]
    pub fn gains(&self) -> [f32; 2] {
        self.gains
    }
}

/// Counters of the output stage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VoiceChatOutputStats {
    /// Frames sent to the mixer.
    pub frames: u64,
    /// Frames dropped because the output fell behind (clock drift).
    pub dropped_frames: u64,
    /// Output streams opened.
    pub streams_opened: u64,
    /// Frames that could not be played (no `Voice` bus, no mixer).
    pub failed_frames: u64,
}

struct LiveStream {
    producer: PcmProducer,
    handle: MediaStreamHandle,
    mixer: SpeakerMixer,
    rate: u32,
    idle_frames: u32,
    occlusion: f32,
    entity_seen: bool,
}

/// A stream that leaves the output (a suspend, a sample-rate change, a replaced entry) stops its
/// feed: an unstopped `Voice`-bus handle keeps counting as voice activity in `Audio`, which holds
/// the `"Voice"` duck on Music for good, and its sound keeps playing silence on the mixer.
impl Drop for LiveStream {
    fn drop(&mut self) {
        self.handle.stop();
    }
}

/// The speakers' output streams. `VoiceChatPlugin` inserts it.
#[derive(Resource, Default)]
pub struct VoiceChatOutput {
    streams: BTreeMap<u16, LiveStream>,
    stats: VoiceChatOutputStats,
    suspend_count: u64,
    /// Scratch for one rendered frame.
    stereo: Vec<f32>,
    /// Senders that produced a frame this tick. Reused so a silent frame does not allocate a set.
    heard: Vec<u16>,
}

impl VoiceChatOutput {
    /// Counters.
    #[must_use]
    pub fn stats(&self) -> VoiceChatOutputStats {
        self.stats
    }

    /// The live speaker output streams, ascending by sender.
    #[must_use]
    pub fn senders(&self) -> Vec<u16> {
        self.streams.keys().copied().collect()
    }

    /// The `(left, right)` gains a speaker is at now.
    #[must_use]
    pub fn gains(&self, sender: u16) -> Option<[f32; 2]> {
        self.streams.get(&sender).map(|s| s.mixer.gains())
    }

    /// Whether a speaker has been placed on an entity.
    #[must_use]
    pub fn is_placed(&self, sender: u16) -> bool {
        self.streams.get(&sender).is_some_and(|s| s.entity_seen)
    }

    fn open(audio: &mut Audio, rate: u32, stats: &mut VoiceChatOutputStats) -> Option<LiveStream> {
        let (mut producer, consumer) = PcmFeed::channel(2, (rate as usize / 50) * (MAX_QUEUED_FRAMES + 4))?;
        let (handle, _bus) = match audio.play_media_feed("Voice", consumer, rate, 1.0) {
            Ok(played) => played,
            Err(error) => {
                tracing::warn!(target: "regia_audio", "voice chat output failed: {error}");
                return None;
            }
        };
        stats.streams_opened += 1;
        let silence = vec![0.0f32; (rate as usize / 50) * 2 * PREBUFFER_FRAMES];
        producer.push(&silence);
        Some(LiveStream {
            producer,
            handle,
            mixer: SpeakerMixer::new(rate),
            rate,
            idle_frames: 0,
            occlusion: 0.0,
            entity_seen: false,
        })
    }
}

/// 20 ms voice frames waiting in `producer` (a stereo feed at `rate` Hz): `queued()` counts
/// interleaved samples, so one voice frame is `rate / 50` frames times the channel count.
fn frames_queued(producer: &PcmProducer, rate: u32) -> usize {
    producer.queued() / ((rate as usize / 50) * usize::from(producer.channels())).max(1)
}

/// `PostUpdate`/`AudioSync`: moves the played frames to the `Voice` bus, placed on their entities,
/// and keeps [`VoiceChatSpeaking`] up to date.
pub(super) fn voice_chat_output(
    mut commands: Commands,
    session: Res<VoiceChatResource>,
    mut audio: Option<ResMut<Audio>>,
    mut output: ResMut<VoiceChatOutput>,
    mut occlusion: Option<ResMut<AudioOcclusion>>,
    emitters: Query<(
        Entity,
        &VoiceChatEmitter,
        &GlobalTransform,
        Option<&VoiceChatSpeaking>,
    )>,
) {
    let _span = tracing::trace_span!("voice_chat_output").entered();
    let mut session = session.lock_session();
    let output = &mut *output;
    update_speaking(&mut commands, &session, &emitters);
    let Some(audio) = audio.as_deref_mut() else {
        session.set_output_tap(false);
        return;
    };
    session.set_output_tap(true);
    // An output rebuild ends every sound on the old mixer: reopen on the next frame.
    if audio.suspend_count() != output.suspend_count {
        output.suspend_count = audio.suspend_count();
        output.streams.clear();
    }
    let places = placements(&session, audio, output, occlusion.as_deref_mut(), &emitters);
    output.heard.clear();
    for frame in session.played() {
        let sender = frame.0;
        let rate = frame.1;
        output.heard.push(sender);
        let live = output.streams.get_mut(&sender).filter(|s| s.rate == rate);
        if live.is_none() {
            output.streams.remove(&sender);
            match VoiceChatOutput::open(audio, rate, &mut output.stats) {
                Some(stream) => {
                    output.streams.insert(sender, stream);
                }
                None => {
                    output.stats.failed_frames += 1;
                    continue;
                }
            }
        }
        let Some(stream) = output.streams.get_mut(&sender) else {
            continue;
        };
        let placement = places.get(&sender).copied().unwrap_or(VoicePlacement::Center);
        stream.idle_frames = 0;
        stream.entity_seen |= matches!(placement, VoicePlacement::Spatial { .. });
        output.stereo.clear();
        stream.mixer.render(&frame.2, placement, &mut output.stereo);
        if frames_queued(&stream.producer, stream.rate) >= MAX_QUEUED_FRAMES {
            output.stats.dropped_frames += 1;
            continue;
        }
        stream.producer.push(&output.stereo);
        output.stats.frames += 1;
    }
    session.acknowledge_output();
    let VoiceChatOutput { streams, heard, .. } = output;
    streams.retain(|sender, stream| {
        if !heard.contains(sender) {
            stream.idle_frames += 1;
        }
        let keep = stream.idle_frames < STREAM_IDLE_FRAMES;
        if !keep {
            stream.handle.stop();
        }
        keep
    });
}

fn update_speaking(
    commands: &mut Commands,
    session: &VoiceChatSession,
    emitters: &Query<(
        Entity,
        &VoiceChatEmitter,
        &GlobalTransform,
        Option<&VoiceChatSpeaking>,
    )>,
) {
    for (entity, emitter, _, marker) in emitters {
        let talking = session
            .speaker(emitter.sender)
            .filter(|s| !s.muted && matches!(s.state(), SpeakerState::Speaking | SpeakerState::Draining));
        match (talking, marker) {
            (Some(speaker), _) => {
                let level = 10f32.powf(speaker.level_db() / 20.0).clamp(0.0, 1.0);
                if marker.is_none_or(|m| (m.level - level).abs() > 0.01) {
                    commands.entity(entity).insert(VoiceChatSpeaking { level });
                }
            }
            (None, Some(_)) => {
                commands.entity(entity).remove::<VoiceChatSpeaking>();
            }
            (None, None) => {}
        }
    }
}

/// Where each active speaker goes this frame. Speakers ascend by sender (spec § 7).
fn placements(
    session: &VoiceChatSession,
    audio: &Audio,
    output: &mut VoiceChatOutput,
    occlusion: Option<&mut AudioOcclusion>,
    emitters: &Query<(
        Entity,
        &VoiceChatEmitter,
        &GlobalTransform,
        Option<&VoiceChatSpeaking>,
    )>,
) -> BTreeMap<u16, VoicePlacement> {
    let mut places = BTreeMap::new();
    let mut rays = Vec::new();
    let listeners: Vec<ListenerFrame> = audio.listeners().iter().map(|(_, f)| *f).collect();
    let tuning = occlusion.as_ref().map(|o| o.tuning).unwrap_or_default();
    for (sender, _) in session.iter_speakers() {
        let channel = session
            .speaker_channel(sender)
            .and_then(|id| session.settings().channels.iter().find(|c| c.id == id));
        let Some(channel) = channel.filter(|c| c.spatial) else {
            places.insert(sender, VoicePlacement::Center);
            continue;
        };
        let found = emitters
            .iter()
            .filter(|(_, e, ..)| e.sender == sender)
            .min_by_key(|(entity, ..)| *entity);
        let Some((entity, emitter, global, _)) = found else {
            places.insert(sender, VoicePlacement::Unresolved);
            continue;
        };
        let params = voice_emitter_params(emitter, channel.radius);
        let frame = EmitterFrame {
            position: global.translation(),
            velocity: Vec3::ZERO,
            forward: Vec3::ZERO,
        };
        let index = nearest_listener(&listeners, frame.position);
        let listener = index.and_then(|i| listeners.get(i)).copied().unwrap_or_default();
        let reported = occlusion
            .as_ref()
            .filter(|_| emitter.occlusion)
            .map_or(0.0, |o| o.reported(entity));
        let smoothed = match output.streams.get_mut(&sender) {
            Some(stream) => {
                stream.occlusion = tuning.smooth(stream.occlusion, reported, FRAME_SECONDS);
                stream.occlusion
            }
            None => reported,
        };
        let cutoff = (smoothed > 0.0).then(|| tuning.cutoff_hz(smoothed));
        let placed = voice_placement(&listener, &frame, &params, tuning.gain(smoothed), cutoff);
        places.insert(sender, placed);
        if emitter.occlusion && (frame.position - listener.position).length() < params.max_distance {
            rays.push(OcclusionRay {
                emitter: entity,
                listener: index.and_then(|i| audio.listeners().get(i)).and_then(|(e, _)| *e),
                from: listener.position,
                to: frame.position,
            });
        }
    }
    if let Some(occlusion) = occlusion {
        occlusion.set_extra_rays(rays);
    }
    places
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The output feed is stereo: one 20 ms voice frame is `rate / 50` *frames*, twice as many
    /// samples. Counting samples halved the drop threshold (concurrency audit 11-03 L3).
    #[test]
    fn queued_voice_frames_count_stereo_frames() {
        let rate = 48_000u32;
        let (mut producer, _consumer) = PcmFeed::channel(2, (rate as usize / 50) * (MAX_QUEUED_FRAMES + 4))
            .expect("stereo is a valid channel count");
        producer.push(&vec![0.0f32; (rate as usize / 50) * 2 * PREBUFFER_FRAMES]);
        assert_eq!(frames_queued(&producer, rate), PREBUFFER_FRAMES);
    }

    /// A stream dropped from the output (the suspend path clears the map, a sample-rate change
    /// removes the entry) releases the `"Voice"` duck on Music: before, the dropped handle was
    /// never stopped and Music stayed at −6 dB for the rest of the session.
    #[test]
    fn a_dropped_output_stream_releases_the_voice_duck() {
        let mut audio = Audio::new(&crate::settings::AudioSettings {
            headless: true,
            ..crate::settings::AudioSettings::default()
        });
        let music_offset = |audio: &Audio| {
            audio
                .bus_status()
                .into_iter()
                .find(|bus| bus.name == "Music")
                .and_then(|bus| bus.applied_offset_db())
                .expect("Music has an applied level")
        };
        let mut stats = VoiceChatOutputStats::default();
        let stream = VoiceChatOutput::open(&mut audio, 16_000, &mut stats).expect("opens on the Voice bus");
        for _ in 0..12 {
            audio.tick(0.02);
        }
        let ducked = music_offset(&audio);
        assert!(
            (ducked + 6.0).abs() < 1e-3,
            "a live voice stream ducks Music: {ducked}"
        );
        drop(stream);
        for _ in 0..60 {
            audio.tick(0.02);
        }
        let released = music_offset(&audio);
        assert!(
            released.abs() < 1e-3,
            "the dropped stream releases the duck, got {released}"
        );
    }
}
