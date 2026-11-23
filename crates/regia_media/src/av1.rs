// SPDX-License-Identifier: MPL-2.0
//! AV1 decoding (`V_AV1`) through `rav1d`, the Rust port of dav1d (ADR-1593, ADR-1869).
//!
//! This module uses `rav1d`'s safe Rust API (the `dav1d-rs` shaped `Decoder` and `Picture`), not its C
//! ABI: a bug inside the decoder then unwinds as an ordinary panic, which the isolation guard catches
//! in development builds, instead of aborting at an `extern "C"` boundary. No `unsafe` code lives here.
//! Every picture is copied out into an owned [`DecodedPicture`] with bounds-checked row reads, so no
//! decoder memory outlives a call.
//!
//! Settings: up to two worker threads from a process-wide budget ([`THREAD_BUDGET`] is the ceiling,
//! 4 on desktop and 2 on Android; the live pool is [`THREADS`], `(cores / 4).clamp(1, 4)` on desktop
//! and 2 on Android, specs/50 § 2.4). A decoder past the budget decodes on its session's thread alone,
//! frame delay 1 (each temporal unit returns its picture in the same call, so lockstep stays exact and
//! `drain` is a formality), the highest spatial layer only, film grain applied, and a frame-size limit
//! taken from the caps so an oversized sequence header is refused before the decoder allocates.
//!
//! AV1 reconstruction is normative, so the output is bit-identical on every target (specs/50 § 7); the
//! tests compare against hashes from two independent decoders.

use std::sync::Arc;

use rav1d::pixel::{ColorPrimaries, TransferCharacteristic, YUVRange};
use rav1d::{Decoder, Picture, PixelLayout, PlanarImageComponent, Rav1dError, Settings};

use crate::codec::{ChromaFormat, ColorInfo, DecodedPicture, VideoDecoder};
use crate::container::{Packet, TrackInfo};
use crate::error::MediaError;
use crate::isolate::DecodeCaps;

/// Upper bound on decoder round trips for one packet (a temporal unit yields at most a handful of
/// pictures with frame delay 1; this only stops a misbehaving decoder from spinning).
const MAX_ROUNDS: usize = 64;

/// The decoder's name in logs, stats and `video.codecs`.
pub const NAME: &str = "rav1d";

/// Ceiling on AV1 worker threads (4 on desktop, 2 on Android). The live pool is [`THREADS`].
pub const THREAD_BUDGET: u32 = if cfg!(target_os = "android") { 2 } else { 4 };

/// Threads per decoder when the budget allows.
const THREADS_PER_DECODER: u32 = 2;

/// A shared count of decoder worker threads. Every decoder gets at least one thread (its own calling
/// thread when the budget is spent), so playback never blocks on the budget.
#[derive(Debug)]
pub struct ThreadBudget {
    in_use: std::sync::atomic::AtomicU32,
    limit: u32,
}

impl ThreadBudget {
    /// A budget of `limit` threads.
    pub const fn new(limit: u32) -> Self {
        Self {
            in_use: std::sync::atomic::AtomicU32::new(0),
            limit,
        }
    }

    /// Takes up to `THREADS_PER_DECODER` (two) threads, at least one; give them back with [`Self::release`].
    pub fn take(&self) -> u32 {
        use std::sync::atomic::Ordering;
        let mut cur = self.in_use.load(Ordering::Relaxed);
        loop {
            let free = self.limit.saturating_sub(cur);
            let grant = free.clamp(1, THREADS_PER_DECODER);
            match self
                .in_use
                .compare_exchange_weak(cur, cur + grant, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return grant,
                Err(now) => cur = now,
            }
        }
    }

    /// Returns `n` threads taken with [`Self::take`].
    pub fn release(&self, n: u32) {
        let _ = self.in_use.try_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Relaxed,
            |cur| Some(cur.saturating_sub(n)),
        );
    }

    /// Threads currently taken.
    pub fn in_use(&self) -> u32 {
        self.in_use.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// The process-wide budget every [`Av1Decoder`] draws from. Desktop uses
/// [`crate::isolate::DecodeCaps::av1_process_budget`]; Android stays at 2.
pub static THREADS: std::sync::LazyLock<ThreadBudget> = std::sync::LazyLock::new(|| {
    let cores = std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(1);
    ThreadBudget::new(crate::isolate::DecodeCaps::av1_process_budget(
        cfg!(target_os = "android"),
        cores,
    ))
});

/// An AV1 decoder for one track.
pub struct Av1Decoder {
    dec: Decoder,
    /// Worker threads taken from [`THREADS`] (returned on drop).
    threads: u32,
    frame_size_limit: u32,
    /// Sequence header and metadata OBUs from the track's `av1C` CodecPrivate, sent before the first
    /// packet after open or flush (a stream may rely on them instead of repeating them in-band).
    config_obus: Vec<u8>,
    send_config: bool,
    track_color: ColorInfo,
}

impl std::fmt::Debug for Av1Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Av1Decoder")
            .field("config_obus", &self.config_obus.len())
            .field("frame_size_limit", &self.frame_size_limit)
            .finish_non_exhaustive()
    }
}

/// The configOBUs of an `av1C` record (AV1 Codec ISO Media File Format Binding § 2.3): a 4-byte header
/// whose first byte is `marker(1) version(7) = 0x81`, then zero or more OBUs. Anything else is ignored.
pub fn av1c_config_obus(codec_private: &[u8]) -> &[u8] {
    match codec_private {
        [0x81, _, _, _, rest @ ..] => rest,
        _ => &[],
    }
}

fn error(what: &str, e: Rav1dError) -> MediaError {
    MediaError::DecodeFailed(format!("AV1 {what}: {e:?}"))
}

fn open_decoder(frame_size_limit: u32, threads: u32) -> Result<Decoder, MediaError> {
    let mut settings = Settings::new();
    settings.set_n_threads(threads);
    settings.set_max_frame_delay(1);
    settings.set_all_layers(false);
    settings.set_apply_grain(true);
    settings.set_frame_size_limit(frame_size_limit);
    Decoder::with_settings(&settings).map_err(|e| error("decoder did not open", e))
}

impl Av1Decoder {
    /// Opens a decoder for `track`; `caps` bound the picture size inside the decoder as well.
    pub fn new(track: &TrackInfo, caps: &DecodeCaps) -> Result<Self, MediaError> {
        let frame_size_limit = caps.max_width.saturating_mul(caps.max_height);
        let threads = THREADS.take();
        let dec = match open_decoder(frame_size_limit, threads) {
            Ok(dec) => dec,
            Err(e) => {
                THREADS.release(threads);
                return Err(e);
            }
        };
        Ok(Self {
            dec,
            threads,
            frame_size_limit,
            config_obus: av1c_config_obus(&track.codec_private).to_vec(),
            send_config: true,
            track_color: track.video.as_ref().map(|v| v.color).unwrap_or_default(),
        })
    }

    /// Replaces the decoder with a fresh one (after it was left holding unsent data).
    fn reset(&mut self) {
        match open_decoder(self.frame_size_limit, self.threads) {
            Ok(dec) => self.dec = dec,
            Err(e) => tracing::warn!("AV1 decoder reset failed, keeping the old one: {e}"),
        }
        self.send_config = true;
    }

    /// Pulls every picture that is ready.
    fn receive(&mut self, out: &mut Vec<DecodedPicture>) -> Result<(), MediaError> {
        for _ in 0..MAX_ROUNDS {
            match self.dec.get_picture() {
                Ok(pic) => out.push(copy_picture(&pic, self.track_color)?),
                Err(Rav1dError::TryAgain) => return Ok(()),
                Err(e) => return Err(error("decode error", e)),
            }
        }
        Err(MediaError::DecodeFailed(
            "the AV1 decoder returned too many pictures for one packet".into(),
        ))
    }

    fn send(&mut self, bytes: &[u8], pts_ns: i64, out: &mut Vec<DecodedPicture>) -> Result<(), MediaError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut sent = self.dec.send_data(bytes.into(), None, Some(pts_ns), None);
        for _ in 0..MAX_ROUNDS {
            match sent {
                Ok(()) => return self.receive(out),
                // The decoder holds pictures it must hand out before it takes the rest of the data.
                Err(Rav1dError::TryAgain) => {
                    self.receive(out)?;
                    sent = self.dec.send_pending_data();
                }
                Err(e) => return Err(error("bitstream error", e)),
            }
        }
        // Unsent data would make the next `send_data` refuse: start over with a fresh decoder.
        self.reset();
        Err(MediaError::DecodeFailed(
            "the AV1 decoder did not consume the packet".into(),
        ))
    }
}

/// Copies one plane row by row into a tightly packed buffer of `w * bytes` per row, bounds-checked.
///
/// The returned handle is the picture's plane. A later GPU submit clones it instead of copying
/// the samples again. `Arc::from(Vec)` copies once here because an `Arc` header cannot adopt a
/// `Vec` allocation in safe Rust; that copy stays on the decode thread.
fn copy_plane(
    plane: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    bytes: usize,
) -> Result<Arc<[u8]>, MediaError> {
    let row = w * bytes;
    let mut out = Vec::with_capacity(row * h);
    for y in 0..h {
        let start = y * stride;
        let src = plane
            .get(start..start + row)
            .ok_or_else(|| MediaError::DecodeFailed("AV1 picture plane shorter than its size".into()))?;
        if bytes == 2 && cfg!(target_endian = "big") {
            for pair in src.as_chunks::<2>().0 {
                out.extend_from_slice(&u16::from_ne_bytes(*pair).to_le_bytes());
            }
        } else {
            out.extend_from_slice(src);
        }
    }
    Ok(Arc::from(out))
}

/// H.273 matrix implied by the primaries: the bitstream's own matrix getter panics on reserved values,
/// so the matrix follows the primaries (BT.709, BT.601 or BT.2020) and falls back to the container.
fn color_of(pic: &Picture, track: ColorInfo) -> ColorInfo {
    let primaries = pic.color_primaries() as u8;
    let matrix = match pic.color_primaries() {
        ColorPrimaries::BT709 => 1,
        ColorPrimaries::BT470BG | ColorPrimaries::ST170M => 6,
        ColorPrimaries::BT2020 => 9,
        _ => track.matrix,
    };
    let transfer = match pic.transfer_characteristic() {
        TransferCharacteristic::Unspecified => track.transfer,
        t => t as u8,
    };
    ColorInfo {
        matrix,
        transfer,
        primaries: if primaries == 2 {
            track.primaries
        } else {
            primaries
        },
        full_range: pic.color_range() == YUVRange::Full,
    }
}

fn copy_picture(pic: &Picture, track_color: ColorInfo) -> Result<DecodedPicture, MediaError> {
    let bad = |why: &str| MediaError::DecodeFailed(format!("AV1 picture {why}"));
    let (w, h) = (pic.width(), pic.height());
    if w == 0 || h == 0 {
        return Err(bad("with no size"));
    }
    let bit_depth = pic.bits_per_component().map_or(8, |b| b.0);
    if !matches!(bit_depth, 8 | 10 | 12) {
        return Err(bad("with a bad bit depth"));
    }
    let bytes = if pic.bit_depth() > 8 { 2 } else { 1 };
    let layout = pic.pixel_layout();
    let chroma = match layout {
        PixelLayout::I420 | PixelLayout::I400 => ChromaFormat::Yuv420,
        PixelLayout::I422 => ChromaFormat::Yuv422,
        PixelLayout::I444 => ChromaFormat::Yuv444,
    };
    let (cw, ch) = chroma.chroma_size(w, h);
    let (w, h, cw, ch) = (w as usize, h as usize, cw as usize, ch as usize);
    let stride = |c| pic.stride(c) as usize;
    let luma = copy_plane(
        pic.plane(PlanarImageComponent::Y),
        stride(PlanarImageComponent::Y),
        w,
        h,
        bytes,
    )?;
    let (u, v) = if layout == PixelLayout::I400 {
        // Monochrome: neutral chroma so every consumer sees 4:2:0.
        let mid = 1u16 << (bit_depth - 1);
        let plane: Arc<[u8]> = if bytes == 2 {
            Arc::from(mid.to_le_bytes().repeat(cw * ch))
        } else {
            Arc::from(vec![mid as u8; cw * ch])
        };
        (Arc::clone(&plane), plane)
    } else {
        let s = stride(PlanarImageComponent::U);
        (
            copy_plane(pic.plane(PlanarImageComponent::U), s, cw, ch, bytes)?,
            copy_plane(pic.plane(PlanarImageComponent::V), s, cw, ch, bytes)?,
        )
    };
    Ok(DecodedPicture {
        pts_ns: pic.timestamp().unwrap_or(0),
        width: w as u32,
        height: h as u32,
        bit_depth,
        chroma,
        color: color_of(pic, track_color),
        planes: [luma, u, v],
    })
}

impl Drop for Av1Decoder {
    fn drop(&mut self) {
        THREADS.release(self.threads);
    }
}

impl VideoDecoder for Av1Decoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<DecodedPicture>, MediaError> {
        let _span = tracing::trace_span!("video_decode", codec = NAME, bytes = packet.data.len()).entered();
        let mut out = Vec::new();
        if self.send_config && !self.config_obus.is_empty() {
            let config = std::mem::take(&mut self.config_obus);
            let sent = self.send(&config, packet.pts_ns, &mut out);
            self.config_obus = config;
            sent?;
        }
        self.send_config = false;
        self.send(&packet.data, packet.pts_ns, &mut out)?;
        Ok(out)
    }

    fn drain(&mut self) -> Result<Vec<DecodedPicture>, MediaError> {
        let mut out = Vec::new();
        self.receive(&mut out)?;
        Ok(out)
    }

    fn flush(&mut self) {
        self.dec.flush();
        // Pictures still queued belong to the old position.
        for _ in 0..MAX_ROUNDS {
            if self.dec.get_picture().is_err() {
                break;
            }
        }
        self.send_config = true;
    }

    fn name(&self) -> &'static str {
        NAME
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av1c_header_is_stripped() {
        assert_eq!(av1c_config_obus(&[0x81, 0, 0x0c, 0, 0x0a, 0x0b]), &[0x0a, 0x0b]);
        assert!(av1c_config_obus(&[0x81, 0, 0, 0]).is_empty());
        assert!(av1c_config_obus(&[0x12, 0, 0, 0, 1]).is_empty());
        assert!(av1c_config_obus(&[]).is_empty());
    }

    #[test]
    fn thread_budget_grants_two_then_one_and_returns_them() {
        let b = ThreadBudget::new(4);
        assert_eq!((b.take(), b.take()), (2, 2));
        assert_eq!(b.take(), 1, "past the budget a decoder still gets its own thread");
        assert_eq!(b.in_use(), 5);
        b.release(2);
        assert_eq!(b.take(), 1);
        b.release(1);
        b.release(1);
        assert_eq!(b.take(), 2);
        let android = ThreadBudget::new(2);
        assert_eq!((android.take(), android.take()), (2, 1));
        android.release(99);
        assert_eq!(android.in_use(), 0);
    }

    #[test]
    fn plane_copy_is_bounds_checked() {
        let plane = [1u8, 2, 3, 9, 4, 5, 6, 9];
        assert_eq!(
            &*copy_plane(&plane, 4, 3, 2, 1).expect("fits"),
            &[1u8, 2, 3, 4, 5, 6][..]
        );
        assert!(copy_plane(&plane, 4, 3, 3, 1).is_err());
        assert!(copy_plane(&plane, 4, 2, 2, 2).is_ok());
    }
}
