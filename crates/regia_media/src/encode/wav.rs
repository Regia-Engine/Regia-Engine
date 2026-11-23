// SPDX-License-Identifier: MPL-2.0
//! A RIFF/WAVE reader for the encoders: PCM 16, 24 and 32 bit and IEEE float 32, any channel count up
//! to 8, read whole into interleaved `f32`. Original code from the public RIFF WAVE layout (the `fmt `
//! and `data` chunks, `WAVE_FORMAT_PCM` 1, `WAVE_FORMAT_IEEE_FLOAT` 3 and `WAVE_FORMAT_EXTENSIBLE`
//! 0xFFFE with its sub-format GUID's first two bytes; `docs/provenance.md`). The file is untrusted:
//! chunk sizes are bounded by the bytes present.

use super::EncodeError;

/// Interleaved sound read from a WAV.
#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    /// Samples per second.
    pub rate: u32,
    /// Channels.
    pub channels: u16,
    /// Interleaved samples in `-1.0..=1.0`.
    pub samples: Vec<f32>,
}

impl Pcm {
    /// Samples per channel.
    pub fn frames(&self) -> u64 {
        (self.samples.len() / usize::from(self.channels.max(1))) as u64
    }
}

fn bad(message: impl Into<String>) -> EncodeError {
    EncodeError::new("encode_bad_input", format!("wav: {}", message.into()))
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Parses a whole WAV file. A `data` chunk whose size runs past the file (a WAV still being written,
/// or the `0xFFFFFFFF` streaming size) is read to the end of the file.
///
/// # Errors
/// `encode_bad_input` for anything that is not a PCM 16/24/32 or float 32 WAV of 1..=8 channels.
pub fn parse(bytes: &[u8]) -> Result<Pcm, EncodeError> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad("not a RIFF WAVE file"));
    }
    let mut at = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = le32(bytes, at + 4) as usize;
        let body = at + 8;
        let end = body.saturating_add(size).min(bytes.len());
        if id == b"fmt " {
            if end - body < 16 {
                return Err(bad("fmt chunk too short"));
            }
            let mut tag = le16(bytes, body);
            let channels = le16(bytes, body + 2);
            let rate = le32(bytes, body + 4);
            let bits = le16(bytes, body + 14);
            if tag == 0xFFFE {
                if end - body < 40 {
                    return Err(bad("extensible fmt chunk too short"));
                }
                tag = le16(bytes, body + 24);
            }
            fmt = Some((tag, channels, rate, bits));
        } else if id == b"data" {
            let (tag, channels, rate, bits) = fmt.ok_or_else(|| bad("data chunk before fmt"))?;
            return decode(tag, channels, rate, bits, &bytes[body..end]);
        }
        // Chunks are padded to an even size.
        at = body.saturating_add(size).saturating_add(size & 1);
    }
    Err(bad("no data chunk"))
}

fn decode(tag: u16, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Result<Pcm, EncodeError> {
    if !(1..=8).contains(&channels) {
        return Err(bad(format!("{channels} channels (1..=8)")));
    }
    if !(1_000..=384_000).contains(&rate) {
        return Err(bad(format!("sample rate {rate}")));
    }
    let width = match (tag, bits) {
        (1, 16) => 2,
        (1, 24) => 3,
        (1 | 3, 32) => 4,
        _ => {
            return Err(bad(format!(
                "format {tag} at {bits} bits (PCM 16/24/32 or float 32)"
            )));
        }
    };
    let frame = width * usize::from(channels);
    let usable = data.len() / frame * frame;
    let samples = data[..usable]
        .chunks_exact(width)
        .map(|s| match (tag, width) {
            (1, 2) => f32::from(i16::from_le_bytes([s[0], s[1]])) / 32_768.0,
            (1, 3) => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
            (1, _) => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
            _ => {
                let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 }
            }
        })
        .collect();
    Ok(Pcm {
        rate,
        channels,
        samples,
    })
}

/// A 16-bit PCM WAV of `samples` (tests and tools).
pub fn write_pcm16(rate: u32, channels: u16, samples: &[f32]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm16_round_trips() {
        let samples = [0.0, 0.5, -0.5, 1.0, -1.0, 0.25];
        let pcm = parse(&write_pcm16(48_000, 2, &samples)).expect("wav");
        assert_eq!((pcm.rate, pcm.channels, pcm.frames()), (48_000, 2, 3));
        for (a, b) in pcm.samples.iter().zip(samples) {
            assert!((a - b).abs() < 1e-4, "{a} vs {b}");
        }
    }

    #[test]
    fn float_and_24_bit_data_decode() {
        let mut wav = write_pcm16(8_000, 1, &[]);
        // Rewrite as float 32 with two samples.
        wav[20..22].copy_from_slice(&3u16.to_le_bytes());
        wav[34..36].copy_from_slice(&32u16.to_le_bytes());
        wav[40..44].copy_from_slice(&8u32.to_le_bytes());
        wav.extend_from_slice(&0.75f32.to_le_bytes());
        wav.extend_from_slice(&f32::NAN.to_le_bytes());
        let pcm = parse(&wav).expect("float");
        assert_eq!(pcm.samples, vec![0.75, 0.0]);
        let mut wav = write_pcm16(8_000, 1, &[]);
        wav[34..36].copy_from_slice(&24u16.to_le_bytes());
        wav[40..44].copy_from_slice(&3u32.to_le_bytes());
        wav.extend_from_slice(&[0x00, 0x00, 0xC0]);
        let pcm = parse(&wav).expect("24-bit");
        assert_eq!(pcm.samples, vec![-0.5]);
    }

    #[test]
    fn hostile_files_are_refused_not_panicking() {
        assert!(parse(b"RIFF").is_err());
        assert!(parse(b"RIFF\0\0\0\0WAVE").is_err());
        let mut wav = write_pcm16(48_000, 2, &[0.1, 0.2]);
        wav[22..24].copy_from_slice(&0u16.to_le_bytes());
        assert!(parse(&wav).is_err());
        let mut wav = write_pcm16(48_000, 2, &[0.1, 0.2]);
        wav[34..36].copy_from_slice(&12u16.to_le_bytes());
        assert!(parse(&wav).is_err());
        // A data size past the end reads what is there.
        let mut wav = write_pcm16(48_000, 1, &[0.1, 0.2]);
        wav[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse(&wav).expect("streaming size").frames(), 2);
        for cut in 0..44 {
            let _ = parse(&write_pcm16(48_000, 1, &[0.1])[..cut]);
        }
    }
}
