// SPDX-License-Identifier: MPL-2.0
//! The decoder benchmark (ADR-1593 § 2, ADR-1670): decodes a clip through the isolation seam on the
//! calling thread and reports time per frame, the slowest frame and the largest decoded picture, so a
//! candidate decoder can be judged on the target devices against the budget of specs/50 § 10. Run it
//! with `cargo run -p regia_media --release --example decode_bench -- [file.webm] [frames]`.

use std::io::Cursor;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::codec::DecodedPicture;
use crate::error::MediaError;
use crate::reference::picture_to_rgba8;
use crate::session::{AudioChoice, FeedPoll, FrameFeed, Session, SessionOptions};

/// One benchmark run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchReport {
    /// Decoder name.
    pub decoder: String,
    /// Picture size.
    pub width: u32,
    /// Picture size.
    pub height: u32,
    /// Pictures decoded.
    pub frames: u64,
    /// Packets read (all tracks).
    pub packets: u64,
    /// Average decode time per picture (demux included), ms.
    pub decode_ms_avg: f64,
    /// Slowest picture, ms.
    pub decode_ms_max: f64,
    /// Average CPU reference YUV to RGB time per picture, ms (0 when skipped).
    pub convert_ms_avg: f64,
    /// Largest decoded picture in bytes.
    pub picture_bytes_max: usize,
    /// Packets the guard rejected.
    pub corrupt: u64,
    /// The specs/50 § 10 budget for this size, ms per frame (22 for 1080p, scaled by pixels).
    pub budget_ms: f64,
}

impl BenchReport {
    /// Whether the average decode time is inside the budget.
    pub fn within_budget(&self) -> bool {
        self.decode_ms_avg <= self.budget_ms
    }
}

/// Decodes up to `max_frames` pictures of `bytes`; `convert` also times the CPU reference conversion.
pub fn run(bytes: Vec<u8>, max_frames: u64, convert: bool) -> Result<BenchReport, MediaError> {
    let opts = SessionOptions {
        audio: AudioChoice::None,
        ..SessionOptions::default()
    };
    let mut session = Session::open(Cursor::new(bytes), &opts, None)?;
    let decoder = session.decoder_name().to_string();
    let mut frames = 0u64;
    let mut total = 0.0f64;
    let mut worst = 0.0f64;
    let mut convert_total = 0.0f64;
    let mut size = (0, 0);
    let mut biggest = 0usize;
    while frames < max_frames {
        let start = Instant::now();
        let poll = session.next_picture();
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        let picture: DecodedPicture = match poll {
            FeedPoll::Frame(p) => p,
            FeedPoll::End | FeedPoll::Pending => break,
            FeedPoll::Failed(e) => return Err(e),
        };
        frames += 1;
        total += ms;
        worst = worst.max(ms);
        size = (picture.width, picture.height);
        biggest = biggest.max(picture.byte_size());
        if convert {
            let start = Instant::now();
            let rgba = picture_to_rgba8(&picture);
            convert_total += start.elapsed().as_secs_f64() * 1000.0;
            std::hint::black_box(rgba);
        }
    }
    let stats = session.stats();
    let pixels = f64::from(size.0) * f64::from(size.1);
    let n = frames.max(1) as f64;
    Ok(BenchReport {
        decoder,
        width: size.0,
        height: size.1,
        frames,
        packets: stats.packets_read,
        decode_ms_avg: total / n,
        decode_ms_max: worst,
        convert_ms_avg: convert_total / n,
        picture_bytes_max: biggest,
        corrupt: stats.video.corrupt,
        budget_ms: 22.0 * (pixels / (1920.0 * 1080.0)).max(1e-6),
    })
}

/// One audio benchmark run (specs/50 § 10: decode plus resample under 1 ms per 20 ms of audio).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioBenchReport {
    /// Matroska codec id of the track.
    pub codec: String,
    /// Output channels.
    pub channels: u8,
    /// Output rate.
    pub sample_rate: u32,
    /// Packets decoded.
    pub packets: u64,
    /// Seconds of audio decoded.
    pub media_s: f64,
    /// Decode time per 20 ms of audio.
    pub ms_per_20ms: f64,
    /// The budget for `ms_per_20ms`.
    pub budget_ms: f64,
}

/// Decodes the default audio track of `bytes` packet by packet and reports the cost per 20 ms of audio.
pub fn run_audio(bytes: Vec<u8>) -> Result<AudioBenchReport, MediaError> {
    use crate::container::{Demuxer, MkvDemuxer};
    let mut demux = MkvDemuxer::open(Cursor::new(bytes))?;
    let track = demux
        .info()
        .default_audio_track("")
        .cloned()
        .ok_or_else(|| MediaError::invalid("no audio track"))?;
    let mut decoder = crate::codec::audio_decoder_for(&track)?;
    let (channels, sample_rate) = decoder.output_format().unwrap_or_else(|| {
        let a = track.audio.as_ref();
        (
            a.map_or(1, |a| a.channels),
            a.map_or(48_000, |a| a.sample_rate.round() as u32),
        )
    });
    let mut out = Vec::new();
    let (mut packets, mut samples, mut total) = (0u64, 0usize, 0.0f64);
    while let Some(p) = demux.next_packet()? {
        if p.track != track.number {
            continue;
        }
        out.clear();
        let start = Instant::now();
        decoder.decode(&p, &mut out)?;
        total += start.elapsed().as_secs_f64() * 1000.0;
        packets += 1;
        samples += out.len();
    }
    let media_s = samples as f64 / f64::from(channels.max(1)) / f64::from(sample_rate.max(1));
    Ok(AudioBenchReport {
        codec: track.codec_id,
        channels,
        sample_rate,
        packets,
        media_s,
        ms_per_20ms: if media_s > 0.0 {
            total / (media_s / 0.02)
        } else {
            0.0
        },
        budget_ms: 1.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::{PatternClip, ToneAudio};

    #[test]
    fn audio_bench_counts_media_time() {
        let mut clip = PatternClip::new(16, 8, 10, 25);
        clip.audio = Some(ToneAudio {
            rate: 48_000,
            channels: 2,
            freq_hz: 440.0,
            amplitude: 0.5,
        });
        let r = run_audio(clip.to_webm()).expect("audio bench");
        assert_eq!(
            (r.codec.as_str(), r.channels, r.sample_rate),
            ("A_PCM/FLOAT/IEEE", 2, 48_000)
        );
        assert!((r.media_s - 0.4).abs() < 0.05, "{}", r.media_s);
        assert!(r.packets >= 10);
        assert!(
            run_audio(PatternClip::new(16, 8, 2, 25).to_webm()).is_err(),
            "no audio track"
        );
    }
}
