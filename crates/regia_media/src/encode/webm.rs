// SPDX-License-Identifier: MPL-2.0
//! The whole encode: pictures in, a finished `.webm` out (ADR-3369).
//!
//! [`WebmEncoder::create`] opens an AV1 encoder and a spool file `<out>.av1.part`. Each pushed picture
//! goes to the encoder, and every temporal unit it emits is appended to the spool, so memory stays
//! flat however long the movie is. [`WebmEncoder::finish`] drains the encoder, encodes the sound (if
//! any) to Opus, and interleaves both by time into `<out>.part` through [`WebmWriter`], which is
//! renamed to `<out>` once complete. The spool is removed whatever happens, also when the encoder is
//! dropped without `finish`.
//!
//! So a finished or cancelled job leaves a complete, playable file of every picture pushed before
//! `finish` (render-out drains the encoder on cancel, like the `ffmpeg` formats), and a process killed
//! mid-encode leaves no `.webm`, only `.part` files.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use super::av1::{Av1Encoder, EncodedFrame};
use super::opus::{OpusPacket, OpusStreamEncoder, SEEK_PRE_ROLL_NS};
use super::wav::Pcm;
use super::{EncodeError, EncodeReport, EncodeSettings, frame_time_ns, rgba_to_i420};
use crate::container::TrackKind;
use crate::mux::{WebmBlock, WebmColour, WebmTrack, WebmWriter};

/// Video track number in written files.
pub const VIDEO_TRACK: u64 = 1;
/// Audio track number in written files.
pub const AUDIO_TRACK: u64 = 2;
/// The `MuxingApp` / `WritingApp` of written files.
pub const WRITING_APP: &str = "Regia Engine regia_media";

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Encodes pictures (and optionally sound) into one WebM file.
#[derive(Debug)]
pub struct WebmEncoder {
    path: PathBuf,
    spool_path: PathBuf,
    part_path: PathBuf,
    spool: Option<BufWriter<File>>,
    encoder: Av1Encoder,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    settings: EncodeSettings,
    codec_private: Vec<u8>,
    units: u64,
    keyframes: u64,
}

impl WebmEncoder {
    /// Opens the encoder for `width` x `height` pictures at `fps_num / fps_den`, writing `path`.
    ///
    /// # Errors
    /// The picture and settings checks, or `encode_io` when the spool cannot be created.
    pub fn create(
        path: &Path,
        width: u32,
        height: u32,
        fps_num: u32,
        fps_den: u32,
        settings: &EncodeSettings,
    ) -> Result<Self, EncodeError> {
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        let encoder = Av1Encoder::new(
            width,
            height,
            fps_num,
            fps_den,
            settings,
            settings.worker_threads(cores),
        )?;
        let spool_path = with_suffix(path, ".av1.part");
        let spool = File::create(&spool_path).map_err(|e| EncodeError::io(&spool_path, &e))?;
        Ok(Self {
            path: path.to_path_buf(),
            part_path: with_suffix(path, ".part"),
            spool_path,
            spool: Some(BufWriter::with_capacity(1 << 20, spool)),
            codec_private: encoder.codec_private(),
            encoder,
            width,
            height,
            fps_num,
            fps_den,
            settings: settings.clone(),
            units: 0,
            keyframes: 0,
        })
    }

    /// Pictures pushed so far.
    pub fn frames(&self) -> u64 {
        self.encoder.sent()
    }

    /// Pushes one 8-bit 4:2:0 picture.
    ///
    /// # Errors
    /// `encode_bad_input` for a wrong-sized picture, `encode_failed` or `encode_io`.
    pub fn push_i420(&mut self, yuv: &[u8]) -> Result<(), EncodeError> {
        let _span = tracing::debug_span!("media_encode_frame").entered();
        self.encoder.send_i420(yuv)?;
        self.drain()
    }

    /// Pushes one 8-bit sRGB RGBA picture (converted with [`rgba_to_i420`]).
    ///
    /// # Errors
    /// As [`Self::push_i420`].
    pub fn push_rgba(&mut self, rgba: &[u8]) -> Result<(), EncodeError> {
        let yuv = rgba_to_i420(rgba, self.width, self.height).ok_or_else(|| {
            EncodeError::new(
                "encode_bad_input",
                format!(
                    "an RGBA {}x{} picture is {} bytes, got {}",
                    self.width,
                    self.height,
                    self.width as usize * self.height as usize * 4,
                    rgba.len()
                ),
            )
        })?;
        self.push_i420(&yuv)
    }

    fn drain(&mut self) -> Result<(), EncodeError> {
        while let Some(unit) = self.encoder.receive()? {
            self.spool_unit(&unit)?;
        }
        Ok(())
    }

    fn spool_unit(&mut self, unit: &EncodedFrame) -> Result<(), EncodeError> {
        if unit.frame != self.units {
            return Err(EncodeError::new(
                "encode_failed",
                format!(
                    "encoder emitted picture {} where {} was due",
                    unit.frame, self.units
                ),
            ));
        }
        let spool = self
            .spool
            .as_mut()
            .ok_or_else(|| EncodeError::new("encode_failed", "spool closed"))?;
        let len = u32::try_from(unit.data.len())
            .map_err(|_| EncodeError::new("encode_failed", "unit over 4 GiB"))?;
        let mut head = [0u8; 5];
        head[0] = u8::from(unit.keyframe);
        head[1..].copy_from_slice(&len.to_le_bytes());
        spool
            .write_all(&head)
            .and_then(|()| spool.write_all(&unit.data))
            .map_err(|e| EncodeError::io(&self.spool_path, &e))?;
        self.units += 1;
        self.keyframes += u64::from(unit.keyframe);
        Ok(())
    }

    /// Ends the stream and writes the file: the encoder is drained, `audio` (if any) encoded to Opus
    /// at [`EncodeSettings::audio_kbps`], both interleaved, the file renamed into place.
    ///
    /// # Errors
    /// `encode_audio_format` for sound Opus cannot take, `encode_failed` or `encode_io`; nothing is
    /// left at the output path on error.
    pub fn finish(mut self, audio: Option<&Pcm>) -> Result<EncodeReport, EncodeError> {
        let _span = tracing::debug_span!("media_encode_finish", frames = self.encoder.sent()).entered();
        self.encoder.flush();
        self.drain()?;
        let spool = self
            .spool
            .take()
            .ok_or_else(|| EncodeError::new("encode_failed", "spool closed"))?;
        spool
            .into_inner()
            .map_err(|e| EncodeError::io(&self.spool_path, e.error()))?
            .sync_data()
            .map_err(|e| EncodeError::io(&self.spool_path, &e))?;
        let (opus, packets) = match audio {
            Some(pcm) if !pcm.samples.is_empty() => {
                let mut enc = OpusStreamEncoder::new(pcm.rate, pcm.channels, self.settings.audio_kbps)?;
                let packets = enc.encode_all(&pcm.samples)?;
                (Some((enc, pcm.frames(), pcm.rate)), packets)
            }
            _ => (None, Vec::new()),
        };
        let report = self.mux(opus.as_ref().map(|(e, n, r)| (e, *n, *r)), &packets);
        if report.is_err() {
            let _ = std::fs::remove_file(&self.part_path);
        }
        report
    }

    fn mux(
        &self,
        opus: Option<(&OpusStreamEncoder, u64, u32)>,
        packets: &[OpusPacket],
    ) -> Result<EncodeReport, EncodeError> {
        let frame_ns = (u64::from(self.fps_den) * 1_000_000_000).div_ceil(u64::from(self.fps_num));
        let mut tracks = vec![WebmTrack {
            number: VIDEO_TRACK,
            kind: TrackKind::Video,
            codec_id: crate::codec::AV1.to_string(),
            codec_private: self.codec_private.clone(),
            codec_delay_ns: 0,
            seek_pre_roll_ns: 0,
            default_duration_ns: Some(frame_ns),
            language: "und".into(),
            video: Some((self.width, self.height, Some(WebmColour::SRGB_BT709_LIMITED))),
            audio: None,
        }];
        if let Some((enc, _, _)) = opus {
            tracks.push(WebmTrack {
                number: AUDIO_TRACK,
                kind: TrackKind::Audio,
                codec_id: crate::codec::OPUS.to_string(),
                codec_private: enc.opus_head(),
                codec_delay_ns: enc.codec_delay_ns(),
                seek_pre_roll_ns: SEEK_PRE_ROLL_NS,
                default_duration_ns: Some(super::opus::PACKET_NS as u64),
                language: "und".into(),
                video: None,
                audio: Some((f64::from(super::opus::OPUS_CLOCK), enc.channels())),
            });
        }
        let part = File::create(&self.part_path).map_err(|e| EncodeError::io(&self.part_path, &e))?;
        let io = |e: std::io::Error| EncodeError::io(&self.part_path, &e);
        let mut writer =
            WebmWriter::new(BufWriter::with_capacity(1 << 20, part), WRITING_APP, &tracks).map_err(io)?;
        let spool = File::open(&self.spool_path).map_err(|e| EncodeError::io(&self.spool_path, &e))?;
        let mut spool = BufReader::with_capacity(1 << 20, spool);
        let mut audio = packets.iter().peekable();
        for k in 0..self.units {
            let mut head = [0u8; 5];
            spool
                .read_exact(&mut head)
                .map_err(|e| EncodeError::io(&self.spool_path, &e))?;
            let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
            let mut data = vec![0u8; len];
            spool
                .read_exact(&mut data)
                .map_err(|e| EncodeError::io(&self.spool_path, &e))?;
            let pts_ns = frame_time_ns(k, self.fps_num, self.fps_den);
            // Sound before the picture only when its millisecond is strictly earlier: at equal times
            // the picture goes first, so a keyframe opens its cluster.
            while let Some(p) = audio.next_if(|p| p.pts_ns / 1_000_000 < pts_ns / 1_000_000) {
                writer.write(&audio_block(p)).map_err(io)?;
            }
            writer
                .write(&WebmBlock {
                    track: VIDEO_TRACK,
                    pts_ns,
                    keyframe: head[0] != 0,
                    discard_padding_ns: 0,
                    data,
                })
                .map_err(io)?;
        }
        for p in audio {
            writer.write(&audio_block(p)).map_err(io)?;
        }
        let video_ns = frame_time_ns(self.units, self.fps_num, self.fps_den) as u64;
        let audio_ns = opus.map_or(0, |(_, frames, rate)| frames * 1_000_000_000 / u64::from(rate));
        let duration_ns = video_ns.max(audio_ns);
        let (out, stats) = writer.finish(duration_ns).map_err(io)?;
        out.into_inner()
            .map_err(|e| EncodeError::io(&self.part_path, e.error()))?
            .sync_all()
            .map_err(|e| EncodeError::io(&self.part_path, &e))?;
        super::replace_finished_part(&self.part_path, &self.path)
            .map_err(|e| EncodeError::io(&self.path, &e))?;
        Ok(EncodeReport {
            path: self.path.display().to_string(),
            frames: self.units,
            keyframes: self.keyframes,
            duration_ns,
            bytes: stats.bytes,
            audio_packets: packets.len() as u64,
            audio_samples: opus.map_or(0, |(_, frames, _)| frames),
            clusters: stats.clusters,
            cues: stats.cues,
        })
    }
}

fn audio_block(p: &OpusPacket) -> WebmBlock {
    WebmBlock {
        track: AUDIO_TRACK,
        pts_ns: p.pts_ns,
        keyframe: true,
        discard_padding_ns: p.discard_padding_ns,
        data: p.data.clone(),
    }
}

impl Drop for WebmEncoder {
    fn drop(&mut self) {
        drop(self.spool.take());
        let _ = std::fs::remove_file(&self.spool_path);
        let _ = std::fs::remove_file(&self.part_path);
    }
}
