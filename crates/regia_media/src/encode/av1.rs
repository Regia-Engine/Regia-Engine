// SPDX-License-Identifier: MPL-2.0
//! The AV1 encoder: `rav1e` (BSD-2-Clause, Rust) behind a small safe wrapper (ADR-3369).
//!
//! 8-bit 4:2:0 pictures go in (tightly packed Y, U, V; [`super::rgba_to_i420`] makes them from RGBA),
//! temporal units come out in presentation order, one per input picture: `rav1e` folds hidden
//! alt-ref frames into the unit of the picture that shows them, so each unit has exactly one shown
//! frame and its input index. The track's `av1C` CodecPrivate comes from
//! [`Av1Encoder::codec_private`]. No `unsafe` here; `rav1e`'s own assembly is off (x86 kernels need
//! NASM, ADR-1593 § 5), so the encoder runs its Rust kernels on every host.
//!
//! Settings: constant quantizer or a target bitrate (single pass), a speed preset, a keyframe interval
//! (the encoder may also place keyframes on scene cuts), and a worker count. `rav1e` with one worker
//! is fully deterministic: two encodes of the same input are byte-identical.

use rav1e::color::{
    ChromaSamplePosition, ChromaSampling, ColorDescription, ColorPrimaries, MatrixCoefficients, PixelRange,
    TransferCharacteristics,
};
use rav1e::data::{FrameType, Rational};
use rav1e::{Config, Context, EncoderConfig, EncoderStatus};

use super::{EncodeError, EncodeSettings, check_picture, i420_bytes};

/// The encoder's name in reports.
pub const NAME: &str = "rav1e";

/// One encoded temporal unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Index of the input picture this unit shows (0-based).
    pub frame: u64,
    /// The shown frame is a keyframe (a WebM sync point).
    pub keyframe: bool,
    /// The temporal unit's OBUs (Section 5 low-overhead format, no temporal delimiter).
    pub data: Vec<u8>,
}

/// An AV1 encoder for one stream of same-sized 8-bit 4:2:0 pictures.
pub struct Av1Encoder {
    ctx: Context<u8>,
    width: usize,
    height: usize,
    sent: u64,
    flushed: bool,
    done: bool,
}

impl std::fmt::Debug for Av1Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Av1Encoder")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("sent", &self.sent)
            .finish_non_exhaustive()
    }
}

fn failed(message: impl Into<String>) -> EncodeError {
    EncodeError::new("encode_failed", message)
}

impl Av1Encoder {
    /// An encoder for `width` x `height` pictures at `fps_num / fps_den`, with `threads` workers
    /// ([`EncodeSettings::worker_threads`]).
    ///
    /// # Errors
    /// A [`check_picture`] or [`EncodeSettings::check`] refusal, or `encode_failed` when `rav1e`
    /// rejects the configuration.
    pub fn new(
        width: u32,
        height: u32,
        fps_num: u32,
        fps_den: u32,
        settings: &EncodeSettings,
        threads: usize,
    ) -> Result<Self, EncodeError> {
        check_picture(width, height, fps_num, fps_den)?;
        settings.check()?;
        let mut enc = EncoderConfig::with_speed_preset(settings.speed);
        enc.width = width as usize;
        enc.height = height as usize;
        enc.bit_depth = 8;
        enc.chroma_sampling = ChromaSampling::Cs420;
        enc.chroma_sample_position = ChromaSamplePosition::Unknown;
        enc.pixel_range = PixelRange::Limited;
        enc.color_description = Some(ColorDescription {
            color_primaries: ColorPrimaries::BT709,
            transfer_characteristics: TransferCharacteristics::SRGB,
            matrix_coefficients: MatrixCoefficients::BT709,
        });
        enc.time_base = Rational::new(u64::from(fps_den), u64::from(fps_num));
        let interval = u64::from(settings.keyframe_interval);
        enc.max_key_frame_interval = interval;
        enc.min_key_frame_interval = interval.min(12);
        enc.quantizer = usize::from(settings.quality);
        enc.min_quantizer = 0;
        enc.bitrate = i32::try_from(u64::from(settings.bitrate_kbps) * 1000).unwrap_or(i32::MAX);
        enc.low_latency = false;
        let cfg = Config::new()
            .with_encoder_config(enc)
            .with_threads(threads.max(1));
        let ctx = cfg
            .new_context::<u8>()
            .map_err(|e| failed(format!("rav1e refused the settings: {e}")))?;
        Ok(Self {
            ctx,
            width: width as usize,
            height: height as usize,
            sent: 0,
            flushed: false,
            done: false,
        })
    }

    /// The `av1C` box (AV1-ISOBMFF § 2.3: the 4-byte configuration record and the sequence header
    /// OBU), the WebM track's CodecPrivate.
    pub fn codec_private(&self) -> Vec<u8> {
        self.ctx.container_sequence_header()
    }

    /// Pictures sent so far.
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// Sends one picture (tightly packed 8-bit Y, U, V planes of the configured size).
    ///
    /// # Errors
    /// `encode_bad_input` for a buffer of the wrong size or a send after [`Self::flush`];
    /// `encode_failed` when the encoder fails.
    pub fn send_i420(&mut self, yuv: &[u8]) -> Result<(), EncodeError> {
        let _span = tracing::trace_span!("media_encode_av1_send").entered();
        let (w, h) = (self.width, self.height);
        if yuv.len() != i420_bytes(w as u32, h as u32) {
            return Err(EncodeError::new(
                "encode_bad_input",
                format!(
                    "a {w}x{h} picture is {} bytes, got {}",
                    i420_bytes(w as u32, h as u32),
                    yuv.len()
                ),
            ));
        }
        if self.flushed {
            return Err(EncodeError::new(
                "encode_bad_input",
                "picture sent after the end of the stream",
            ));
        }
        let (cw, ch) = (w / 2, h / 2);
        let mut frame = self.ctx.new_frame();
        frame.planes[0].copy_from_raw_u8(&yuv[..w * h], w, 1);
        frame.planes[1].copy_from_raw_u8(&yuv[w * h..w * h + cw * ch], cw, 1);
        frame.planes[2].copy_from_raw_u8(&yuv[w * h + cw * ch..], cw, 1);
        self.ctx
            .send_frame(frame)
            .map_err(|e| failed(format!("rav1e send: {e}")))?;
        self.sent += 1;
        Ok(())
    }

    /// Ends the stream: the encoder emits everything it holds.
    pub fn flush(&mut self) {
        if !self.flushed {
            self.ctx.flush();
            self.flushed = true;
        }
    }

    /// The next encoded unit, if one is ready. Before [`Self::flush`] `None` means "send more";
    /// after it, `None` means the stream is complete.
    ///
    /// # Errors
    /// `encode_failed` when the encoder fails.
    pub fn receive(&mut self) -> Result<Option<EncodedFrame>, EncodeError> {
        let _span = tracing::trace_span!("media_encode_av1_receive").entered();
        if self.done {
            return Ok(None);
        }
        loop {
            match self.ctx.receive_packet() {
                Ok(packet) => {
                    return Ok(Some(EncodedFrame {
                        frame: packet.input_frameno,
                        keyframe: packet.frame_type == FrameType::KEY,
                        data: packet.data,
                    }));
                }
                Err(EncoderStatus::Encoded) => {}
                Err(EncoderStatus::NeedMoreData) => return Ok(None),
                Err(EncoderStatus::LimitReached) => {
                    self.done = true;
                    return Ok(None);
                }
                Err(e) => return Err(failed(format!("rav1e: {e}"))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_sizes_are_refused_before_the_encoder_starts() {
        let s = EncodeSettings::default();
        let code = |w, h| Av1Encoder::new(w, h, 24, 1, &s, 1).err().map(|e| e.code);
        assert_eq!(code(0, 0), Some("encode_bad_size"));
        assert_eq!(code(129, 72), Some("encode_odd_size"));
        assert_eq!(code(9000, 64), Some("encode_too_large"));
        let bad = EncodeSettings {
            keyframe_interval: 100_000,
            ..EncodeSettings::default()
        };
        assert_eq!(
            Av1Encoder::new(128, 72, 24, 1, &bad, 1).err().map(|e| e.code),
            Some("encode_bad_keyframe_interval")
        );
    }

    #[test]
    fn a_wrong_sized_picture_is_refused_and_the_stream_ends_cleanly() {
        let mut e = Av1Encoder::new(32, 32, 24, 1, &EncodeSettings::default(), 1).expect("encoder");
        assert_eq!(
            e.send_i420(&[0; 10]).err().map(|e| e.code),
            Some("encode_bad_input")
        );
        assert_eq!(e.codec_private()[0], 0x81, "av1C marker and version 1");
        e.send_i420(&vec![128; i420_bytes(32, 32)]).expect("picture");
        e.flush();
        assert_eq!(
            e.send_i420(&vec![128; i420_bytes(32, 32)]).err().map(|e| e.code),
            Some("encode_bad_input")
        );
        let mut units = Vec::new();
        while let Some(u) = e.receive().expect("receive") {
            units.push(u);
        }
        assert_eq!(units.len(), 1);
        assert!(units[0].keyframe && units[0].frame == 0);
        assert_eq!(e.receive().expect("done"), None);
    }
}
