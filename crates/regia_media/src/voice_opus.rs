// SPDX-License-Identifier: MPL-2.0
//! Opus for voice chat through `unsafe-libopus`, libopus 1.3.1 translated to unsafe Rust (ADR-1593,
//! ADR-1911, `docs/specs/59-voice-chat.md` §§ 2.8, 3).
//!
//! One [`OpusVoiceCodec`] is one encoder and one decoder for one mono stream at a voice profile's
//! rate: voice application, 20 ms frames, in-band forward error correction on, the codec's own DTX off
//! (silence is the session's job, spec § 2.2), VBR constrained to the profile's target. [`OpusCodecs`]
//! is the provider a session uses; [`VoiceOpusPlugin`] installs it.
//!
//! Hard caps before every call into the translated code: only the packet shape this encoder produces
//! reaches the decoder ([`check_packet`]), the decoder only ever gets a buffer of one frame, and
//! encoder input is clamped and stripped of non-finite samples.
//!
//! **The library's packet-loss concealment is never called (ADR-1911).** In `unsafe-libopus` 0.2.0
//! `silk_PLC_conceal` lost the `(opus_int16)` cast of `silk_int16_MIN` in translation: the frame's
//! inner saturation reads `if x < 0x8000 { 0x8000 }`, so concealed SILK output pins at +1.0 and stays
//! in the decoder's state. Every route to that code is closed here: a lost frame is concealed in Rust
//! ([`Concealer`], pitch repeat with a fade), in-band FEC runs only when the packet really carries
//! LBRR data ([`has_lbrr`]), and [`check_packet`] refuses the packets libopus would treat as lost
//! (one- and two-byte packets, empty frames) or that would make it conceal a SILK tail (a CELT-only
//! packet after SILK). Move back to the library's concealment once a release fixes the cast.

use regia_app::{App, Plugin};
use regia_audio::voice_chat::{
    MAX_PAYLOAD, SharedCodecProvider, VoiceChatCodecs, VoiceChatError, VoiceChatResource, VoiceCodec,
    VoiceCodecId, VoiceCodecProvider, VoiceQuality, new_codec, tail_period,
};
use std::sync::Arc;
use unsafe_libopus::{
    OPUS_APPLICATION_VOIP, OPUS_OK, OPUS_RESET_STATE, OPUS_SET_BITRATE_REQUEST, OPUS_SET_COMPLEXITY_REQUEST,
    OPUS_SET_DTX_REQUEST, OPUS_SET_INBAND_FEC_REQUEST, OPUS_SET_PACKET_LOSS_PERC_REQUEST,
    OPUS_SET_SIGNAL_REQUEST, OPUS_SET_VBR_CONSTRAINT_REQUEST, OPUS_SET_VBR_REQUEST, OPUS_SIGNAL_VOICE,
    OpusDecoder, OpusEncoder, opus_decode_float, opus_decoder_create, opus_decoder_ctl, opus_decoder_destroy,
    opus_encode_float, opus_encoder_create, opus_encoder_ctl, opus_encoder_destroy, opus_strerror,
};

/// The loss the encoder sizes its in-band redundancy for until a transport reports a real figure.
const DEFAULT_EXPECTED_LOSS_PCT: u8 = 10;
/// Packets of this size or less carry no audio: libopus would conceal them, so they are a silent frame.
const MAX_EMPTY_PACKET: usize = 2;
/// Gain left after each concealed frame.
const CONCEAL_FADE_PER_FRAME: f32 = 0.75;
/// Crossfade from concealment back into decoded audio, in milliseconds.
const RESUME_CROSSFADE_MS: u32 = 3;
/// Frames after a concealed frame during which the output level is capped (500 ms).
const RECOVERY_FRAMES: u32 = 25;
/// How far above the talker's level the output may go during recovery (+6 dB).
const RECOVERY_HEADROOM: f32 = 2.0;
/// The level cap never falls below this RMS (-30 dBFS), so speech after a quiet stretch stays audible.
const RECOVERY_FLOOR_RMS: f32 = 0.03;
/// How fast the tracked level follows the output, per frame.
const LEVEL_RATE: f32 = 0.1;
/// The same during recovery: a talker who really gets louder is let through over about a second.
const RECOVERY_LEVEL_RATE: f32 = 0.01;

fn frame_rms(frame: &[f32]) -> f32 {
    (frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32).sqrt()
}

/// Whether `payload` (a packet [`check_packet`] accepted) carries LBRR data: the bit after the voice
/// activity bit of a one-frame SILK or hybrid packet's first byte (RFC 6716 § 4.2.3).
#[must_use]
pub fn has_lbrr(payload: &[u8]) -> bool {
    payload.get(1).is_some_and(|b| b & 0x40 != 0)
}

/// Checks that `payload` is the shape this encoder produces: one mono 20 ms SILK or hybrid frame
/// (RFC 6716 § 3.1). Anything else from a peer is refused before the decoder sees it.
///
/// # Errors
/// [`VoiceChatError::Codec`] naming the problem.
pub fn check_packet(payload: &[u8]) -> Result<(), VoiceChatError> {
    let bad = |why: &str| Err(VoiceChatError::Codec(format!("opus packet refused: {why}")));
    if payload.len() > MAX_PAYLOAD {
        return bad("over the payload cap");
    }
    let Some(&toc) = payload.first() else {
        return bad("empty");
    };
    if toc & 0x03 != 0 {
        return bad("more than one frame");
    }
    if toc & 0x04 != 0 {
        return bad("stereo");
    }
    let config = toc >> 3;
    let twenty_ms = match config {
        0..=11 => config & 0x03 == 1,
        12..=15 => config & 0x01 == 1,
        _ => return bad("CELT only"),
    };
    if !twenty_ms {
        return bad("not 20 ms");
    }
    Ok(())
}

/// Packet-loss concealment in Rust: repeats the last pitch period with a per-frame fade, and crossfades
/// back when audio returns. Spec 59 § 2.9 asks for the decoder's concealment; see the module docs.
///
/// The decoder never hears about a concealed frame, so its prediction memory has a gap: on a very
/// periodic signal it rings above the talker's level until the error decays. For a moment after a
/// loss the output level is capped at +6 dB over the tracked level, so a lost packet cannot turn into
/// a loud note.
#[derive(Clone, Debug)]
pub struct Concealer {
    rate: u32,
    history: Vec<f32>,
    period: usize,
    cursor: usize,
    gain: f32,
    lost: bool,
    level: f32,
    recovery: u32,
    limit: f32,
}

impl Concealer {
    /// A concealer for frames of `frame_samples` at `rate` Hz.
    #[must_use]
    pub fn new(rate: u32, frame_samples: usize) -> Self {
        Self {
            rate,
            history: vec![0.0; frame_samples * 2],
            period: 1,
            cursor: 0,
            gain: 1.0,
            lost: false,
            level: 0.0,
            recovery: 0,
            limit: 1.0,
        }
    }

    fn next_sample(&mut self) -> f32 {
        let len = self.history.len();
        let sample = self.history.get(self.cursor).copied().unwrap_or(0.0);
        self.cursor += 1;
        if self.cursor >= len {
            self.cursor = len.saturating_sub(self.period);
        }
        sample
    }

    /// Fills `out` with the next concealed frame.
    pub fn conceal(&mut self, out: &mut [f32]) {
        self.recovery = RECOVERY_FRAMES;
        if !self.lost {
            self.lost = true;
            self.gain = 1.0;
            self.period = tail_period(&self.history, self.rate).max(1);
            self.cursor = self.history.len().saturating_sub(self.period);
        }
        let end_gain = self.gain * CONCEAL_FADE_PER_FRAME;
        let step = (end_gain - self.gain) / out.len().max(1) as f32;
        for o in out.iter_mut() {
            self.gain += step;
            *o = self.next_sample() * self.gain;
        }
        self.gain = end_gain;
    }

    /// Takes a decoded frame: remembers it, and crossfades into it after concealment.
    pub fn accept(&mut self, frame: &mut [f32]) {
        if self.lost {
            let overlap = ((self.rate * RESUME_CROSSFADE_MS / 1000) as usize).min(frame.len());
            for (i, s) in frame.iter_mut().take(overlap).enumerate() {
                let t = (i as f32 + 0.5) / overlap as f32;
                *s = *s * t + self.next_sample() * self.gain * (1.0 - t);
            }
            self.lost = false;
        }
        self.limit_recovery(frame);
        let len = self.history.len();
        let keep = len.saturating_sub(frame.len());
        self.history.copy_within(frame.len().min(len).., 0);
        if let Some(tail) = self.history.get_mut(keep..) {
            tail.copy_from_slice(&frame[frame.len() - tail.len()..]);
        }
    }

    /// Tracks the talker's level and, after a loss, ramps the frame under the cap.
    fn limit_recovery(&mut self, frame: &mut [f32]) {
        let recovering = self.recovery > 0;
        self.recovery = self.recovery.saturating_sub(1);
        let rms = frame_rms(frame);
        let target = if recovering {
            let cap = self.level.max(RECOVERY_FLOOR_RMS) * RECOVERY_HEADROOM;
            if rms > cap { cap / rms } else { 1.0 }
        } else {
            1.0
        };
        let step = (target - self.limit) / frame.len().max(1) as f32;
        for s in frame.iter_mut() {
            self.limit += step;
            *s *= self.limit;
        }
        self.limit = target;
        let rate = if recovering {
            RECOVERY_LEVEL_RATE
        } else {
            LEVEL_RATE
        };
        self.level += (rms * target - self.level) * rate;
    }

    /// Forgets the stream.
    pub fn reset(&mut self) {
        self.history.fill(0.0);
        self.lost = false;
        self.gain = 1.0;
        self.level = 0.0;
        self.recovery = 0;
        self.limit = 1.0;
    }
}

fn codec_error(what: &str, code: i32) -> VoiceChatError {
    VoiceChatError::Codec(format!("{what}: {}", opus_strerror(code)))
}

struct RawEncoder(*mut OpusEncoder);

impl RawEncoder {
    fn open(rate: i32) -> Result<Self, VoiceChatError> {
        let mut err = 0i32;
        // SAFETY: `err` is a live local, read after the call; the rate is one of the profile rates.
        let st = unsafe { opus_encoder_create(rate, 1, OPUS_APPLICATION_VOIP, &mut err) };
        if st.is_null() || err != OPUS_OK {
            return Err(codec_error("opus encoder", err));
        }
        Ok(Self(st))
    }

    fn set(&mut self, request: i32, value: i32) -> Result<(), VoiceChatError> {
        // SAFETY: `self.0` is a live encoder, reached only through `&mut self`.
        let code = unsafe { opus_encoder_ctl!(self.0, request, value) };
        if code == OPUS_OK {
            Ok(())
        } else {
            Err(codec_error("opus encoder setting", code))
        }
    }
}

impl Drop for RawEncoder {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `opus_encoder_create` and is destroyed once, here.
        unsafe { opus_encoder_destroy(self.0) };
    }
}

struct RawDecoder(*mut OpusDecoder);

impl RawDecoder {
    fn open(rate: i32) -> Result<Self, VoiceChatError> {
        let mut err = 0i32;
        // SAFETY: `err` is a live local, read after the call; the rate is one of the profile rates.
        let st = unsafe { opus_decoder_create(rate, 1, &mut err) };
        if st.is_null() || err != OPUS_OK {
            return Err(codec_error("opus decoder", err));
        }
        Ok(Self(st))
    }
}

impl Drop for RawDecoder {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `opus_decoder_create` and is destroyed once, here.
        unsafe { opus_decoder_destroy(self.0) };
    }
}

/// One mono voice stream's Opus encoder and decoder.
pub struct OpusVoiceCodec {
    encoder: RawEncoder,
    decoder: RawDecoder,
    frame_samples: usize,
    input: Vec<f32>,
    concealer: Concealer,
}

// SAFETY: the libopus state is plain memory owned by this value alone (no thread affinity, no shared
// globals), and it is only reached through `&mut self`.
unsafe impl Send for OpusVoiceCodec {}

impl std::fmt::Debug for OpusVoiceCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusVoiceCodec")
            .field("frame_samples", &self.frame_samples)
            .finish_non_exhaustive()
    }
}

impl OpusVoiceCodec {
    /// A codec for one stream at `quality` with the encoder at `complexity` (0..=10).
    ///
    /// # Errors
    /// [`VoiceChatError::InvalidParams`] for a complexity above 10, [`VoiceChatError::Codec`] when
    /// libopus refuses.
    pub fn new(quality: VoiceQuality, complexity: u8) -> Result<Self, VoiceChatError> {
        if complexity > 10 {
            return Err(VoiceChatError::InvalidParams(format!(
                "complexity {complexity} is above 10"
            )));
        }
        let rate = quality.sample_rate() as i32;
        let mut encoder = RawEncoder::open(rate)?;
        encoder.set(OPUS_SET_BITRATE_REQUEST, quality.target_bps() as i32)?;
        encoder.set(OPUS_SET_VBR_REQUEST, 1)?;
        encoder.set(OPUS_SET_VBR_CONSTRAINT_REQUEST, 1)?;
        encoder.set(OPUS_SET_COMPLEXITY_REQUEST, i32::from(complexity))?;
        encoder.set(OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_VOICE)?;
        encoder.set(OPUS_SET_INBAND_FEC_REQUEST, 1)?;
        encoder.set(
            OPUS_SET_PACKET_LOSS_PERC_REQUEST,
            i32::from(DEFAULT_EXPECTED_LOSS_PCT),
        )?;
        encoder.set(OPUS_SET_DTX_REQUEST, 0)?;
        Ok(Self {
            encoder,
            decoder: RawDecoder::open(rate)?,
            frame_samples: quality.frame_samples(),
            input: vec![0.0; quality.frame_samples()],
            concealer: Concealer::new(quality.sample_rate(), quality.frame_samples()),
        })
    }
}

/// The result of a decode call: a valid packet shorter than a voice frame (nothing Regia sends) leaves
/// the rest of the frame silent.
fn decoded(code: i32, out: &mut [f32]) -> Result<usize, VoiceChatError> {
    let Ok(got) = usize::try_from(code) else {
        return Err(codec_error("opus decode", code));
    };
    if let Some(rest) = out.get_mut(got..) {
        rest.fill(0.0);
    }
    Ok(out.len())
}

impl VoiceCodec for OpusVoiceCodec {
    fn encode(&mut self, frame: &[f32], out: &mut [u8]) -> Result<usize, VoiceChatError> {
        let _span = tracing::trace_span!("voice_chat_encode_opus").entered();
        if frame.len() != self.frame_samples {
            return Err(VoiceChatError::Codec(format!(
                "frame of {} samples, expected {}",
                frame.len(),
                self.frame_samples
            )));
        }
        for (d, s) in self.input.iter_mut().zip(frame) {
            *d = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
        }
        let cap = out.len().min(MAX_PAYLOAD);
        // SAFETY: `input` holds `frame_samples` samples (the frame size passed), `out` has at least
        // `cap` bytes and libopus writes at most `cap`; the encoder is reached only through `&mut self`.
        let code = unsafe {
            opus_encode_float(
                self.encoder.0,
                self.input.as_ptr(),
                self.frame_samples as i32,
                out.as_mut_ptr(),
                cap as i32,
            )
        };
        usize::try_from(code).map_err(|_| codec_error("opus encode", code))
    }

    fn decode(&mut self, packet: Option<&[u8]>, fec: bool, out: &mut [f32]) -> Result<usize, VoiceChatError> {
        let _span = tracing::trace_span!("voice_chat_decode").entered();
        let dst = out
            .get_mut(..self.frame_samples)
            .ok_or_else(|| VoiceChatError::Codec("decode buffer is smaller than a frame".into()))?;
        let Some(payload) = packet else {
            self.concealer.conceal(dst);
            return Ok(self.frame_samples);
        };
        if payload.len() <= MAX_EMPTY_PACKET {
            if fec {
                // Nothing to recover from a comfort packet.
                self.concealer.conceal(dst);
            } else {
                dst.fill(0.0);
                self.concealer.accept(dst);
            }
            return Ok(self.frame_samples);
        }
        check_packet(payload)?;
        if fec && !has_lbrr(payload) {
            self.concealer.conceal(dst);
            return Ok(self.frame_samples);
        }
        // SAFETY: `payload` is a live slice of the length passed, one frame of the shape
        // `check_packet` allows; `dst` holds `frame_samples` samples, the most libopus writes for
        // that frame size.
        let code = unsafe {
            opus_decode_float(
                self.decoder.0,
                payload.as_ptr(),
                payload.len() as i32,
                dst.as_mut_ptr(),
                self.frame_samples as i32,
                i32::from(fec),
            )
        };
        let n = decoded(code, dst)?;
        self.concealer.accept(dst);
        Ok(n)
    }

    fn set_target_bitrate(&mut self, bps: u32) {
        let bps = bps.clamp(6_000, 64_000) as i32;
        if let Err(e) = self.encoder.set(OPUS_SET_BITRATE_REQUEST, bps) {
            tracing::warn!(target: "regia_media", "voice bit rate not applied: {e}");
        }
    }

    fn set_expected_loss(&mut self, percent: u8) {
        if let Err(e) = self
            .encoder
            .set(OPUS_SET_PACKET_LOSS_PERC_REQUEST, i32::from(percent.min(100)))
        {
            tracing::warn!(target: "regia_media", "voice loss estimate not applied: {e}");
        }
    }

    fn reset(&mut self) {
        // SAFETY: both states are live and reached only through `&mut self`.
        let (a, b) = unsafe {
            (
                opus_encoder_ctl!(self.encoder.0, OPUS_RESET_STATE),
                opus_decoder_ctl!(self.decoder.0, OPUS_RESET_STATE),
            )
        };
        if a != OPUS_OK || b != OPUS_OK {
            tracing::warn!(target: "regia_media", "opus reset failed: {a} {b}");
        }
        self.concealer.reset();
    }
}

/// The provider that speaks Opus and still decodes PCM from peers without it.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpusCodecs;

impl VoiceCodecProvider for OpusCodecs {
    fn preferred(&self) -> VoiceCodecId {
        VoiceCodecId::Opus
    }

    fn build(
        &self,
        id: VoiceCodecId,
        quality: VoiceQuality,
        complexity: u8,
    ) -> Result<Box<dyn VoiceCodec + Send>, VoiceChatError> {
        match id {
            VoiceCodecId::Opus => Ok(Box::new(OpusVoiceCodec::new(quality, complexity)?)),
            VoiceCodecId::Pcm => new_codec(id, quality, complexity),
        }
    }
}

/// [`OpusCodecs`] as a shared handle, for [`VoiceChatSession::set_codec_provider`].
///
/// [`VoiceChatSession::set_codec_provider`]: regia_audio::voice_chat::VoiceChatSession::set_codec_provider
#[must_use]
pub fn opus_codecs() -> SharedCodecProvider {
    Arc::new(OpusCodecs)
}

/// Makes voice chat speak Opus. Add it before or after `VoiceChatPlugin`.
#[derive(Clone, Copy, Debug, Default)]
pub struct VoiceOpusPlugin;

impl Plugin for VoiceOpusPlugin {
    fn build(&self, app: &mut App) {
        let provider = opus_codecs();
        app.insert_resource(VoiceChatCodecs(Arc::clone(&provider)));
        if let Some(session) = app.world().get_resource::<VoiceChatResource>()
            && let Ok(mut session) = session.0.lock()
            && let Err(error) = session.set_codec_provider(provider)
        {
            tracing::error!(target: "regia_media", "voice chat stays on PCM: {error}");
        }
    }
}
