// SPDX-License-Identifier: MPL-2.0
//! Remote speaker: jitter buffer, decoder, state machine and playout
//! (`docs/specs/59-voice-chat.md` §§ 2.3, 2.9).

use super::codec::{BuiltinCodecs, SharedCodecProvider, VoiceCodec};
use super::jitter::{FRAME_MS, InsertOutcome, JitterBuffer, JitterSettings, JitterStats, PullKind};
use super::packet::{FRAME_TIMESTAMP_STEP, VoiceCodecId, VoiceFlags, VoicePacket, VoiceQuality, parse_multi};
use super::timescale::{lengthen, shorten};
use super::transmit::frame_rms_db;
use std::collections::VecDeque;

/// Frames without a packet (and without `end_of_talk`) before a speaker times out (1 s).
pub const SPEAKER_TIMEOUT_FRAMES: u64 = 50;

/// Frames between two time-scaling steps, so a correction stays inaudible.
const SCALE_SPACING_FRAMES: u32 = 4;

/// State of a remote speaker (spec § 2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeakerState {
    /// Nothing is arriving.
    Silent,
    /// First packet seen, prefilling.
    Buffering,
    /// Audio is playing.
    Speaking,
    /// `end_of_talk` arrived; the buffer plays out.
    Draining,
}

impl SpeakerState {
    /// Lowercase name for the agent protocol.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Silent => "silent",
            Self::Buffering => "buffering",
            Self::Speaking => "speaking",
            Self::Draining => "draining",
        }
    }
}

/// Counters of one speaker.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SpeakerStats {
    /// The jitter buffer's counters.
    pub jitter: JitterStats,
    /// Packets the codec could not decode.
    pub decode_errors: u64,
    /// Times the speaker was forced to `Silent` for lack of packets.
    pub timeouts: u64,
    /// Packets dropped for a codec or layout this build cannot play.
    pub unsupported: u64,
    /// Frames shortened by a pitch period (the buffer was over target).
    pub shortened: u64,
    /// Frames lengthened by a pitch period (the buffer was under target).
    pub lengthened: u64,
    /// Long-run clock drift between this sender and the output, in parts per million: positive when
    /// the sender runs fast (spec § 2.9).
    pub drift_ppm: f32,
}

type StreamCodec = (VoiceCodecId, VoiceQuality, Box<dyn VoiceCodec + Send>);

/// A remote participant.
pub struct VoiceSpeaker {
    /// Sender id (session slot).
    pub sender: u16,
    /// Playback gain, 0.0..=2.0 (0..=200 %).
    pub gain: f32,
    /// Locally muted: packets are dropped before decode.
    pub muted: bool,
    jitter: JitterBuffer,
    jitter_settings: JitterSettings,
    provider: SharedCodecProvider,
    complexity: u8,
    codec: Option<StreamCodec>,
    state: SpeakerState,
    last_packet_frame: u64,
    was_silent: bool,
    level_db: f32,
    stats: SpeakerStats,
    time_scaling: bool,
    fifo: VecDeque<f32>,
    since_scale: u32,
    scaled_samples: i64,
    decoded_samples: u64,
    /// Last rendered frame. Reused by [`Self::render`].
    pcm: Vec<f32>,
    /// Decoded frame before time scaling. Capacity covers one extra pitch period.
    decoded: Vec<f32>,
    /// Payload of the jitter frame just pulled.
    payload_scratch: Vec<u8>,
    /// Set when this render crossed the speaking boundary: `(now_speaking, level_db)`.
    activity_edge: Option<(bool, f32)>,
    /// The last decoded frame was a concealment: the next silence fades it out (spec § 2.9 (3)).
    concealing: bool,
    /// The codec refused a payload since [`Self::take_codec_error`] last ran.
    codec_failed: bool,
}

impl VoiceSpeaker {
    /// A silent speaker decoding with the always-built codecs.
    #[must_use]
    pub fn new(sender: u16, jitter: JitterSettings) -> Self {
        Self::with_provider(sender, jitter, std::sync::Arc::new(BuiltinCodecs), 5)
    }

    /// A silent speaker decoding through `provider`.
    #[must_use]
    pub fn with_provider(
        sender: u16,
        jitter: JitterSettings,
        provider: SharedCodecProvider,
        complexity: u8,
    ) -> Self {
        Self {
            sender,
            gain: 1.0,
            muted: false,
            jitter: JitterBuffer::new(jitter),
            jitter_settings: jitter,
            provider,
            complexity,
            codec: None,
            state: SpeakerState::Silent,
            last_packet_frame: 0,
            was_silent: true,
            level_db: -120.0,
            stats: SpeakerStats::default(),
            time_scaling: false,
            fifo: VecDeque::new(),
            since_scale: SCALE_SPACING_FRAMES,
            scaled_samples: 0,
            decoded_samples: 0,
            pcm: Vec::new(),
            decoded: Vec::new(),
            payload_scratch: Vec::new(),
            activity_edge: None,
            concealing: false,
            codec_failed: false,
        }
    }

    /// Turns time scaling on or off (off in the deterministic mode, spec § 7).
    pub fn set_time_scaling(&mut self, on: bool) {
        self.time_scaling = on;
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> SpeakerState {
        self.state
    }

    /// Frames waiting in the jitter buffer.
    #[must_use]
    pub fn depth_frames(&self) -> usize {
        self.jitter.depth_frames()
    }

    /// Sample rate of the stream being played (the last packet's profile).
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.codec
            .as_ref()
            .map_or(VoiceQuality::default().sample_rate(), |(_, q, _)| q.sample_rate())
    }

    /// RMS level of the last played frame in dBFS.
    #[must_use]
    pub fn level_db(&self) -> f32 {
        self.level_db
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> SpeakerStats {
        let drift_ppm = if self.decoded_samples == 0 {
            0.0
        } else {
            (self.scaled_samples as f64 / self.decoded_samples as f64 * 1e6) as f32
        };
        SpeakerStats {
            jitter: self.jitter.stats(),
            drift_ppm,
            ..self.stats
        }
    }

    /// Discards decoder and buffer (sender id rebound, `reset` flag).
    pub fn reset(&mut self) {
        self.jitter = JitterBuffer::new(self.jitter_settings);
        self.codec = None;
        self.state = SpeakerState::Silent;
        self.was_silent = true;
        self.level_db = -120.0;
        self.fifo.clear();
        self.concealing = false;
    }

    /// Routes a parsed packet into the jitter buffer.
    pub fn receive(&mut self, packet: &VoicePacket<'_>, now_frame: u64) -> Option<InsertOutcome> {
        if self.muted {
            return None;
        }
        if packet.flags.contains(VoiceFlags::RESET) {
            self.reset();
        }
        let wanted = (packet.codec, packet.quality);
        let same = self.codec.as_ref().is_some_and(|(c, q, _)| (*c, *q) == wanted);
        if !same {
            match self.provider.build(packet.codec, packet.quality, self.complexity) {
                Ok(c) => {
                    self.jitter = JitterBuffer::new(self.jitter_settings);
                    self.codec = Some((packet.codec, packet.quality, c));
                }
                Err(_) => {
                    self.stats.unsupported += 1;
                    return None;
                }
            }
        }
        let end = packet.flags.contains(VoiceFlags::END_OF_TALK);
        let outcome = if packet.flags.contains(VoiceFlags::MULTI) {
            let Ok(frames) = parse_multi(packet.payload) else {
                self.stats.unsupported += 1;
                return None;
            };
            let next_seq = packet.seq.wrapping_add(1);
            let next_timestamp = packet.timestamp.wrapping_add(FRAME_TIMESTAMP_STEP);
            if frames.redundant {
                // The first frame is a spare copy of a frame already sent: it only fills a hole.
                let outcome = self
                    .jitter
                    .insert(next_seq, next_timestamp, end, frames.second, now_frame);
                self.jitter.insert_spare(packet.seq, frames.first);
                outcome
            } else {
                let outcome =
                    self.jitter
                        .insert(packet.seq, packet.timestamp, false, frames.first, now_frame);
                self.jitter.insert_following(next_seq, end, frames.second);
                outcome
            }
        } else {
            self.jitter
                .insert(packet.seq, packet.timestamp, end, packet.payload, now_frame)
        };
        self.last_packet_frame = now_frame;
        if end {
            self.state = SpeakerState::Draining;
        } else if self.state == SpeakerState::Silent {
            self.state = SpeakerState::Buffering;
        }
        Some(outcome)
    }

    /// Produces the next 20 ms frame (mono, profile rate). Empty when the speaker is silent.
    pub fn pull_frame(&mut self, now_frame: u64) -> Vec<f32> {
        let mut out = Vec::new();
        self.pull_frame_into(now_frame, &mut out);
        out
    }

    /// [`Self::pull_frame`] into `out`, which keeps its capacity.
    pub fn pull_frame_into(&mut self, now_frame: u64, out: &mut Vec<f32>) {
        out.clear();
        let Some((_, quality, _)) = &self.codec else {
            return;
        };
        let n = quality.frame_samples();
        let waiting = self.state == SpeakerState::Buffering || self.state == SpeakerState::Speaking;
        if waiting && now_frame.saturating_sub(self.last_packet_frame) > SPEAKER_TIMEOUT_FRAMES {
            self.stats.timeouts += 1;
            self.reset();
            return;
        }
        if !self.next_output_into(n, out) {
            self.state = SpeakerState::Silent;
            self.was_silent = true;
            self.level_db = -120.0;
            out.clear();
            return;
        }
        if self.state == SpeakerState::Buffering && !self.was_silent {
            self.state = SpeakerState::Speaking;
        }
        for s in out.iter_mut() {
            *s = if s.is_finite() {
                (*s * self.gain).clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
        self.level_db = frame_rms_db(out);
    }

    /// Renders one frame into [`Self::rendered`] and reports a speaking-state edge.
    pub fn render(&mut self, now_frame: u64) {
        let before = self.state == SpeakerState::Speaking || self.state == SpeakerState::Draining;
        let mut pcm = std::mem::take(&mut self.pcm);
        self.pull_frame_into(now_frame, &mut pcm);
        self.pcm = pcm;
        let after = self.state == SpeakerState::Speaking || self.state == SpeakerState::Draining;
        self.activity_edge = (before != after).then_some((after, self.level_db));
    }

    /// The frame from the last [`Self::render`]. Empty when the speaker produced silence.
    #[must_use]
    pub fn rendered(&self) -> &[f32] {
        &self.pcm
    }

    /// Speaking-state edge from the last [`Self::render`], if the state changed.
    #[must_use]
    pub fn activity_edge(&self) -> Option<(bool, f32)> {
        self.activity_edge
    }

    /// Whether the codec refused a payload (`codec_error`) since the last call; the frame was
    /// concealed instead.
    pub fn take_codec_error(&mut self) -> bool {
        std::mem::take(&mut self.codec_failed)
    }

    /// One decoded frame from the jitter buffer. `false` when the talk spurt is over.
    fn next_decoded_into(&mut self, n: usize, out: &mut Vec<f32>) -> bool {
        let need = n.saturating_add(480);
        if out.capacity() < need {
            out.reserve(need);
        }
        out.clear();
        out.resize(n, 0.0);
        // `scratch` stays local across `decode_into`: the payload and `&mut self` would alias if
        // the bytes were already stored back on the speaker.
        let mut scratch = std::mem::take(&mut self.payload_scratch);
        let kind = self.jitter.pull_into(&mut scratch);
        match kind {
            PullKind::Finished => {
                self.payload_scratch = scratch;
                return false;
            }
            PullKind::Silence if std::mem::take(&mut self.concealing) => {
                // Spec 59 § 2.9 (3): the frame after the fifth concealed one fades the
                // concealment out over the frame (linear, 1 at the first sample to 0 at the
                // last) instead of stepping to silence.
                self.decode_into(None, false, out);
                let last = out.len().saturating_sub(1).max(1) as f32;
                for (i, s) in out.iter_mut().enumerate() {
                    *s *= (last - i as f32).max(0.0) / last;
                }
                self.was_silent = true;
            }
            PullKind::Buffering | PullKind::Silence => self.was_silent = true,
            PullKind::Frame => self.decode_into(Some(&scratch), false, out),
            PullKind::Recover => self.decode_into(Some(&scratch), true, out),
            PullKind::Conceal => self.decode_into(None, false, out),
        }
        self.concealing = kind == PullKind::Conceal;
        self.payload_scratch = scratch;
        if self.jitter.take_spurt_end()
            && let Some((_, _, codec)) = &mut self.codec
        {
            // The talk spurt ended on this frame: the next one starts from a clean decoder, so
            // a loss at its start conceals to silence, never the old utterance's last frame
            // (audit-gameplay-muse M3).
            codec.reset();
        }
        true
    }

    /// `n` output samples. With time scaling a decoded frame may be a pitch period shorter or
    /// longer, so a tick takes two frames from the buffer or none; without it this is one frame.
    fn next_output_into(&mut self, n: usize, out: &mut Vec<f32>) -> bool {
        if !self.time_scaling {
            return self.next_decoded_into(n, out);
        }
        let rate = self.sample_rate();
        while self.fifo.len() < n {
            // The depth the scaling decision compares with the target is the one before this
            // frame is taken from the buffer (read after, an on-target buffer looks one short).
            let level = self.jitter.depth_frames() as u32;
            let target = self.jitter.target_ms() / FRAME_MS;
            let mut decoded = std::mem::take(&mut self.decoded);
            let produced = self.next_decoded_into(n, &mut decoded);
            if !produced {
                self.decoded = decoded;
                if self.fifo.is_empty() {
                    return false;
                }
                while self.fifo.len() < n {
                    self.fifo.push_back(0.0);
                }
                break;
            }
            self.decoded_samples += decoded.len() as u64;
            self.since_scale = self.since_scale.saturating_add(1);
            // Only real speech is scaled: the buffer refills on its own in silence, and a draining
            // talk spurt must not be stretched.
            if self.state == SpeakerState::Speaking
                && !self.was_silent
                && self.since_scale >= SCALE_SPACING_FRAMES
            {
                if level >= target + 2 {
                    let removed = shorten(&mut decoded, rate);
                    self.scaled_samples += removed as i64;
                    self.stats.shortened += u64::from(removed > 0);
                    self.since_scale = 0;
                } else if level < target && level > 0 {
                    let added = lengthen(&mut decoded, rate);
                    self.scaled_samples -= added as i64;
                    self.stats.lengthened += u64::from(added > 0);
                    self.since_scale = 0;
                }
            }
            self.fifo.extend(decoded.iter().copied());
            self.decoded = decoded;
        }
        out.clear();
        if out.capacity() < n {
            out.reserve(n);
        }
        for _ in 0..n {
            if let Some(sample) = self.fifo.pop_front() {
                out.push(sample);
            }
        }
        true
    }

    fn decode_into(&mut self, payload: Option<&[u8]>, fec: bool, out: &mut [f32]) {
        let Some((_, _, codec)) = &mut self.codec else {
            return;
        };
        let result = match codec.decode(payload, fec, out) {
            Ok(n) => Ok(n),
            Err(_) => {
                self.codec_failed = true;
                codec.decode(None, false, out)
            }
        };
        match result {
            Ok(_) => {
                if self.was_silent && payload.is_some() && !fec {
                    // 20 ms crossfade from silence on resume.
                    let n = out.len().max(1) as f32;
                    for (i, s) in out.iter_mut().enumerate() {
                        *s *= i as f32 / n;
                    }
                }
                self.was_silent = false;
            }
            Err(_) => {
                self.stats.decode_errors += 1;
                out.fill(0.0);
            }
        }
    }
}
