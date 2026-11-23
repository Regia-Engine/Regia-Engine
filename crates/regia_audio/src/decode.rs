// SPDX-License-Identifier: MPL-2.0
//! Whole-file decoding with a size and duration cap (threat-model finding 30, ADR-4922).
//!
//! A static sound, a streaming sound decoded to memory for an offline render, and a loudness
//! measurement all hold the whole decoded PCM in memory (8 bytes per frame). A compressed file can
//! decode to far more than it weighs (a FLAC block of silence is a few bytes for 65 535 frames), and
//! its header may claim any length or say nothing. So the decoder here counts the frames it has
//! produced and stops with [`AudioError::DecodeLimit`] before a packet would take the sound past
//! [`SoundDecodeLimits`], instead of trusting the container. Packets are decoded and converted the
//! way the audio library's own whole-file loader does it (default track, mono doubled to both
//! channels, stereo kept, more channels refused), so the frames are the same.

use kira::Frame;
use kira::sound::static_sound::{StaticSoundData, StaticSoundSettings};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::TrackType;
use symphonia::core::io::{MediaSource, MediaSourceStream};

use crate::error::AudioError;

/// Longest sound decoded whole into memory, in seconds (30 minutes). Longer audio streams.
pub const MAX_DECODED_SOUND_SECS: f64 = 1800.0;

/// Most bytes of decoded frames one sound may hold in memory (1 GiB: about 46 minutes of 48 kHz
/// stereo). Binds before [`MAX_DECODED_SOUND_SECS`] for sample rates above about 74.5 kHz.
pub const MAX_DECODED_SOUND_BYTES: u64 = 1 << 30;

/// Highest sample rate a sound file may declare, Hz (384 kHz, the top studio rate). The header
/// field is a `u32`; a crafted file past this is refused with [`AudioError::SampleRateTooHigh`]
/// before any frame is decoded, because loudness windows and resampling buffers are sized from
/// the rate (deep-zai audio-mix finding 12).
pub const MAX_SOUND_SAMPLE_RATE: u32 = 384_000;

/// Bytes one decoded frame takes (two `f32` channels).
const FRAME_BYTES: u64 = std::mem::size_of::<Frame>() as u64;

/// Caps on decoding one sound whole into memory. The default is [`MAX_DECODED_SOUND_SECS`] and
/// [`MAX_DECODED_SOUND_BYTES`]; whichever a sound reaches first stops it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SoundDecodeLimits {
    /// Longest decoded duration, in seconds.
    pub max_secs: f64,
    /// Most bytes of decoded frames (8 bytes per frame).
    pub max_bytes: u64,
}

impl Default for SoundDecodeLimits {
    fn default() -> Self {
        Self {
            max_secs: MAX_DECODED_SOUND_SECS,
            max_bytes: MAX_DECODED_SOUND_BYTES,
        }
    }
}

impl SoundDecodeLimits {
    /// The most frames a sound at `sample_rate` may decode to under both caps (0 for a negative or
    /// NaN duration).
    #[must_use]
    pub fn max_frames(&self, sample_rate: u32) -> u64 {
        let by_bytes = self.max_bytes / FRAME_BYTES;
        let by_secs = self.max_secs * f64::from(sample_rate);
        // `as` saturates: NaN and negatives give 0, +inf gives `u64::MAX`.
        by_bytes.min(by_secs.floor() as u64)
    }
}

fn load_err(label: &str, message: impl std::fmt::Display) -> AudioError {
    AudioError::load(label, message)
}

/// The header's sample rate, refused when unknown, 0 or above [`MAX_SOUND_SAMPLE_RATE`] (a
/// crafted header; checked before anything is sized from it).
fn checked_rate(label: &str, rate: Option<u32>) -> Result<u32, AudioError> {
    let rate = rate
        .filter(|&rate| rate > 0)
        .ok_or_else(|| load_err(label, "unknown sample rate"))?;
    if rate > MAX_SOUND_SAMPLE_RATE {
        return Err(AudioError::SampleRateTooHigh {
            path: label.to_string(),
            sample_rate: rate,
            max: MAX_SOUND_SAMPLE_RATE,
        });
    }
    Ok(rate)
}

/// Decodes `source` whole into memory within `limits`.
///
/// # Errors
/// [`AudioError::LoadFailed`] when the bytes do not decode (no audio track, unknown sample rate,
/// more than two channels, a codec error), [`AudioError::SampleRateTooHigh`] past
/// [`MAX_SOUND_SAMPLE_RATE`], [`AudioError::DecodeLimit`] when the sound would go past `limits`.
pub(crate) fn decode_static(
    source: Box<dyn MediaSource>,
    label: &str,
    limits: &SoundDecodeLimits,
) -> Result<StaticSoundData, AudioError> {
    let _span = tracing::info_span!(target: "regia_audio", "sound_decode", label).entered();
    let stream = MediaSourceStream::new(source, Default::default());
    let mut reader = symphonia::default::get_probe()
        .probe(
            &Default::default(),
            stream,
            Default::default(),
            Default::default(),
        )
        .map_err(|e| load_err(label, e))?;
    let track = reader
        .default_track(TrackType::Audio)
        .ok_or_else(|| load_err(label, "no default audio track"))?;
    let track_id = track.id;
    let Some(CodecParameters::Audio(params)) = track.codec_params.as_ref() else {
        return Err(load_err(label, "no default audio track"));
    };
    // The WAV reader passes a header rate of 0 through. A sound at 0 Hz has no duration
    // (`frames / 0`), and the duration of an empty one is NaN, which panics in
    // `Duration::from_secs_f64`. So 0 is refused here, as an unknown rate is.
    let sample_rate = checked_rate(label, params.sample_rate)?;
    let max_frames = limits.max_frames(sample_rate);
    let limit = || AudioError::DecodeLimit {
        path: label.to_string(),
        max_frames,
        sample_rate,
    };
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &Default::default())
        .map_err(|e| load_err(label, e))?;
    let mut frames: Vec<Frame> = Vec::new();
    let mut samples: Vec<f32> = Vec::new();
    while let Some(packet) = reader.next_packet().map_err(|e| load_err(label, e))? {
        if packet.track_id != track_id {
            continue;
        }
        let buffer = decoder.decode(&packet).map_err(|e| load_err(label, e))?;
        // Checked before the packet's frames are converted or stored.
        let total = (frames.len() as u64).saturating_add(buffer.frames() as u64);
        if total > max_frames {
            return Err(limit());
        }
        let channels = buffer.num_planes();
        buffer.copy_to_vec_interleaved(&mut samples);
        match channels {
            1 => frames.extend(samples.iter().map(|&s| Frame::from_mono(s))),
            2 => frames.extend(samples.as_chunks::<2>().0.iter().map(|[l, r]| Frame::new(*l, *r))),
            _ => return Err(load_err(label, "only mono and stereo sounds are supported")),
        }
    }
    Ok(StaticSoundData {
        sample_rate,
        frames: frames.into(),
        settings: StaticSoundSettings::default(),
        slice: None,
    })
}

/// What a sound file's header declares, read without decoding a frame ([`probe_sound_header`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoundHeader {
    /// The declared sample rate, Hz (never 0, at most [`MAX_SOUND_SAMPLE_RATE`]).
    pub sample_rate: u32,
    /// The declared frame count, when the container states one (a WAV `data` chunk, a FLAC
    /// stream header, the last Ogg page). A claim, not a decode: a cut file holds fewer.
    pub frames: Option<u64>,
}

/// Reads the header of an encoded sound file (WAV, Ogg Vorbis, FLAC, MP3) without decoding a
/// frame, so a size check can run before a whole-file decode (the cook's impulse-response tier
/// check).
///
/// # Errors
/// [`AudioError::LoadFailed`] when the container is not recognised or has no audio track or
/// rate, [`AudioError::SampleRateTooHigh`] past [`MAX_SOUND_SAMPLE_RATE`].
pub fn probe_sound_header(bytes: &[u8], label: &str) -> Result<SoundHeader, AudioError> {
    let stream = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes.to_vec())), Default::default());
    let reader = symphonia::default::get_probe()
        .probe(
            &Default::default(),
            stream,
            Default::default(),
            Default::default(),
        )
        .map_err(|e| load_err(label, e))?;
    let track = reader
        .default_track(TrackType::Audio)
        .ok_or_else(|| load_err(label, "no default audio track"))?;
    let Some(CodecParameters::Audio(params)) = track.codec_params.as_ref() else {
        return Err(load_err(label, "no default audio track"));
    };
    Ok(SoundHeader {
        sample_rate: checked_rate(label, params.sample_rate)?,
        frames: track.num_frames,
    })
}

/// [`decode_static`] over in-memory bytes.
pub(crate) fn decode_static_bytes(
    bytes: Vec<u8>,
    label: &str,
    limits: &SoundDecodeLimits,
) -> Result<StaticSoundData, AudioError> {
    decode_static(Box::new(std::io::Cursor::new(bytes)), label, limits)
}

/// [`decode_static`] over a file.
pub(crate) fn decode_static_file(
    path: &std::path::Path,
    limits: &SoundDecodeLimits,
) -> Result<StaticSoundData, AudioError> {
    let label = path.display().to_string();
    let file = std::fs::File::open(path).map_err(|e| load_err(&label, e))?;
    decode_static(Box::new(file), &label, limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_cap_takes_the_tighter_limit() {
        let limits = SoundDecodeLimits::default();
        // 30 minutes at 48 kHz is under 1 GiB of frames; at 192 kHz the bytes bind.
        assert_eq!(limits.max_frames(48_000), 1800 * 48_000);
        assert_eq!(limits.max_frames(192_000), (1 << 30) / 8);
        let none = SoundDecodeLimits {
            max_secs: f64::NAN,
            max_bytes: 1 << 20,
        };
        assert_eq!(none.max_frames(48_000), 0);
        let bytes_only = SoundDecodeLimits {
            max_secs: f64::INFINITY,
            max_bytes: 80,
        };
        assert_eq!(bytes_only.max_frames(u32::MAX), 10);
    }

    /// A PCM WAV header: 16-bit mono at `rate` (any u32, the byte rate wraps as a crafted header
    /// would), then `frames` samples.
    fn wav(rate: u32, frames: u32) -> Vec<u8> {
        let data_len = frames * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&rate.wrapping_mul(2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            out.extend_from_slice(&((i % 2000) as i16).to_le_bytes());
        }
        out
    }

    /// Deep-zai audio-mix finding 12: a header rate past [`MAX_SOUND_SAMPLE_RATE`] is refused
    /// with a typed error before any frame is decoded (and before a loudness meter sizes its
    /// windows from it); the maximum itself loads.
    #[test]
    fn a_sample_rate_past_the_maximum_is_refused_before_decoding() {
        let limits = SoundDecodeLimits::default();
        for rate in [MAX_SOUND_SAMPLE_RATE + 1, 1_000_000, u32::MAX] {
            let result = decode_static_bytes(wav(rate, 8), "crafted.wav", &limits);
            assert_eq!(
                result.err(),
                Some(AudioError::SampleRateTooHigh {
                    path: "crafted.wav".into(),
                    sample_rate: rate,
                    max: MAX_SOUND_SAMPLE_RATE,
                }),
                "a {rate} Hz header is refused with SampleRateTooHigh"
            );
        }
        let at_max = decode_static_bytes(wav(MAX_SOUND_SAMPLE_RATE, 8), "max.wav", &limits)
            .expect("the documented maximum rate loads");
        assert_eq!(at_max.sample_rate, MAX_SOUND_SAMPLE_RATE);
        assert_eq!(MAX_SOUND_SAMPLE_RATE, 384_000, "the documented maximum");
    }

    /// The loudness path reads the same decode: a crafted rate never reaches `LoudnessMeter`.
    #[test]
    fn a_loudness_measurement_of_a_crafted_rate_is_a_typed_error() {
        let measured = crate::loudness_table::measure_sound_bytes(wav(1_000_000, 64), "crafted.wav");
        assert!(
            matches!(
                measured,
                Err(AudioError::SampleRateTooHigh {
                    sample_rate: 1_000_000,
                    ..
                })
            ),
            "the measurement stops at the decode: {measured:?}"
        );
    }
}
