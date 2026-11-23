// SPDX-License-Identifier: MPL-2.0
//! Voice codec seam and the always-built [`PcmCodec`] (`docs/specs/59-voice-chat.md` § 3).
//!
//! `regia_audio` holds no codec library. Opus lives in `regia_media` behind its `voice-opus`
//! feature (ADR-1593 keeps every media codec there, ADR-1911) and reaches a session through
//! [`VoiceCodecProvider`]; without a provider [`new_codec`] answers
//! [`VoiceChatError::Unsupported`] for Opus and the session transmits PCM.

use super::error::VoiceChatError;
use super::packet::{VoiceCodecId, VoiceQuality};
use std::sync::Arc;

/// One encoder and decoder state for one stream.
pub trait VoiceCodec {
    /// Encodes one 20 ms mono frame into `out` and returns the byte count.
    ///
    /// # Errors
    /// [`VoiceChatError::Codec`] when the frame length is wrong or `out` is too small.
    fn encode(&mut self, frame: &[f32], out: &mut [u8]) -> Result<usize, VoiceChatError>;

    /// Decodes one packet payload into `out` and returns the sample count. `None` asks the
    /// decoder to conceal a lost frame; `fec` asks it to recover the frame *before* `packet`
    /// from `packet`'s in-band redundancy (codecs without FEC conceal instead).
    ///
    /// # Errors
    /// [`VoiceChatError::Codec`] on a malformed payload or a too-small `out`.
    fn decode(&mut self, packet: Option<&[u8]>, fec: bool, out: &mut [f32]) -> Result<usize, VoiceChatError>;

    /// Writes the short comfort payload that closes a talk spurt when no audio frame is left to
    /// send (self mute, device loss) and returns its length. It must decode as one silent frame.
    /// The default is an empty payload.
    ///
    /// # Errors
    /// [`VoiceChatError::Codec`] when `out` is too small.
    fn encode_comfort(&mut self, _out: &mut [u8]) -> Result<usize, VoiceChatError> {
        Ok(0)
    }

    /// Asks for a target bit rate in bits per second (the adaptive bit rate of spec § 2.8).
    /// Codecs with a fixed rate ignore it.
    fn set_target_bitrate(&mut self, _bps: u32) {}

    /// Tells the encoder the loss the receivers report, in percent, so it can size its in-band
    /// redundancy. Codecs without redundancy ignore it.
    fn set_expected_loss(&mut self, _percent: u8) {}

    /// Drops the coding state (stream restart, spec § 2.4).
    fn reset(&mut self) {}
}

/// Builds codecs for a session. The seam that lets a codec library live outside `regia_audio`
/// (ADR-1593, ADR-1911): `regia_media` implements it for Opus; a game may supply its own.
pub trait VoiceCodecProvider: Send + Sync {
    /// The codec new talk spurts are encoded with.
    fn preferred(&self) -> VoiceCodecId;

    /// Builds one stream's codec.
    ///
    /// # Errors
    /// [`VoiceChatError::Unsupported`] for a codec this provider lacks,
    /// [`VoiceChatError::InvalidParams`] for a complexity above 10.
    fn build(
        &self,
        id: VoiceCodecId,
        quality: VoiceQuality,
        complexity: u8,
    ) -> Result<Box<dyn VoiceCodec + Send>, VoiceChatError>;
}

/// The always-built provider: [`PcmCodec`] only.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinCodecs;

impl VoiceCodecProvider for BuiltinCodecs {
    fn preferred(&self) -> VoiceCodecId {
        VoiceCodecId::Pcm
    }

    fn build(
        &self,
        id: VoiceCodecId,
        quality: VoiceQuality,
        complexity: u8,
    ) -> Result<Box<dyn VoiceCodec + Send>, VoiceChatError> {
        new_codec(id, quality, complexity)
    }
}

/// A shared provider handle.
pub type SharedCodecProvider = Arc<dyn VoiceCodecProvider>;

/// Mode byte of a full PCM frame.
const MODE_FRAME: u8 = 0;
/// Mode byte of a 1-byte comfort (silence) frame.
const MODE_COMFORT: u8 = 1;
/// Gain applied to the repeated frame for each consecutive concealed frame.
const CONCEAL_FADE: f32 = 0.7;

/// 16-bit little-endian PCM with a 1-byte mode header. Several times the Opus bit rate: meant
/// for tests, loopback and negotiated fallback. Concealment repeats the last frame with a fade
/// per consecutive loss.
#[derive(Clone, Debug)]
pub struct PcmCodec {
    frame_samples: usize,
    last: Vec<f32>,
    conceal_gain: f32,
}

impl PcmCodec {
    /// A codec for one stream at the profile's frame size.
    #[must_use]
    pub fn new(quality: VoiceQuality) -> Self {
        let frame_samples = quality.frame_samples();
        Self {
            frame_samples,
            last: vec![0.0; frame_samples],
            conceal_gain: 1.0,
        }
    }

    /// Samples per frame.
    #[must_use]
    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    fn conceal(&mut self, out: &mut [f32]) -> Result<usize, VoiceChatError> {
        let dst = out
            .get_mut(..self.frame_samples)
            .ok_or_else(|| VoiceChatError::Codec("decode buffer is smaller than a frame".into()))?;
        self.conceal_gain *= CONCEAL_FADE;
        for (o, l) in dst.iter_mut().zip(&self.last) {
            *o = l * self.conceal_gain;
        }
        Ok(self.frame_samples)
    }
}

impl VoiceCodec for PcmCodec {
    fn encode_comfort(&mut self, out: &mut [u8]) -> Result<usize, VoiceChatError> {
        let slot = out
            .first_mut()
            .ok_or_else(|| VoiceChatError::Codec("output buffer is empty".into()))?;
        *slot = MODE_COMFORT;
        Ok(1)
    }

    fn reset(&mut self) {
        self.last.fill(0.0);
        self.conceal_gain = 1.0;
    }

    fn encode(&mut self, frame: &[f32], out: &mut [u8]) -> Result<usize, VoiceChatError> {
        if frame.len() != self.frame_samples {
            return Err(VoiceChatError::Codec(format!(
                "frame of {} samples, expected {}",
                frame.len(),
                self.frame_samples
            )));
        }
        let need = 1 + 2 * self.frame_samples;
        let out_len = out.len();
        let dst = out
            .get_mut(..need)
            .ok_or_else(|| VoiceChatError::Codec(format!("output buffer of {out_len} bytes, need {need}")))?;
        dst[0] = MODE_FRAME;
        for (chunk, s) in dst[1..].as_chunks_mut::<2>().0.iter_mut().zip(frame) {
            let v = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
            *chunk = ((v * 32767.0).round() as i16).to_le_bytes();
        }
        Ok(need)
    }

    fn decode(&mut self, packet: Option<&[u8]>, fec: bool, out: &mut [f32]) -> Result<usize, VoiceChatError> {
        let _span = tracing::trace_span!("voice_chat_decode").entered();
        let Some(payload) = packet.filter(|_| !fec) else {
            return self.conceal(out);
        };
        let dst = out
            .get_mut(..self.frame_samples)
            .ok_or_else(|| VoiceChatError::Codec("decode buffer is smaller than a frame".into()))?;
        match payload.split_first() {
            // A comfort frame is exactly its mode byte; trailing bytes are malformed traffic and
            // count in `decode_errors` instead of passing as silence (muse L, ADR-1809).
            Some((&MODE_COMFORT, [])) => dst.fill(0.0),
            Some((&MODE_COMFORT, rest)) => {
                return Err(VoiceChatError::Codec(format!(
                    "comfort frame with {} trailing bytes",
                    rest.len()
                )));
            }
            Some((&MODE_FRAME, body)) => {
                if body.len() != 2 * self.frame_samples {
                    return Err(VoiceChatError::Codec(format!(
                        "pcm payload of {} bytes, expected {}",
                        body.len(),
                        2 * self.frame_samples
                    )));
                }
                for (o, c) in dst.iter_mut().zip(body.as_chunks::<2>().0) {
                    *o = f32::from(i16::from_le_bytes(*c)) / 32768.0;
                }
            }
            Some((mode, _)) => return Err(VoiceChatError::Codec(format!("unknown pcm mode {mode}"))),
            None => return Err(VoiceChatError::Codec("empty payload".into())),
        }
        self.last.copy_from_slice(dst);
        self.conceal_gain = 1.0;
        Ok(self.frame_samples)
    }
}

/// Builds an always-built codec for a stream. `complexity` (0..=10) only matters to other
/// providers.
///
/// # Errors
/// [`VoiceChatError::Unsupported`] for Opus (it comes from a [`VoiceCodecProvider`], see the
/// module docs), [`VoiceChatError::InvalidParams`] for a complexity above 10.
pub fn new_codec(
    id: VoiceCodecId,
    quality: VoiceQuality,
    complexity: u8,
) -> Result<Box<dyn VoiceCodec + Send>, VoiceChatError> {
    if complexity > 10 {
        return Err(VoiceChatError::InvalidParams(format!(
            "complexity {complexity} is above 10"
        )));
    }
    match id {
        VoiceCodecId::Pcm => Ok(Box::new(PcmCodec::new(quality))),
        VoiceCodecId::Opus => Err(VoiceChatError::Unsupported(
            "no Opus codec provider is installed (regia_media feature voice-opus)".into(),
        )),
    }
}
