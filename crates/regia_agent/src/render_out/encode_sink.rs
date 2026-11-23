// SPDX-License-Identifier: MPL-2.0
//! Format `webm` (ADR-3369): render-out frames encoded in-process to AV1 in WebM, with the audio
//! mixdown as Opus, through `regia_media::encode` (feature `media-encode`; no external tool).
//!
//! The app thread never encodes: delivered RGBA frames go through the same bounded, interruptible
//! queue as the `ffmpeg` feeder (`Slots`, ADR-2396) to one encoder thread, which converts each frame
//! to BT.709 4:2:0 and feeds the AV1 encoder (itself multi-threaded, `encode.threads`). Encoded units
//! are spooled to `<name>.webm.av1.part`. Closing the sink ends the feed; the encoder thread drains
//! the encoder and hands it back, and the closing thread (after the WAV mixdown is closed over the
//! delivered frames) encodes the mixdown to Opus and writes `<name>.webm`.
//!
//! Cancel and failure: like the `ffmpeg` formats, a cancelled job's movie holds every frame delivered
//! before the cancel (the encoder is drained, not dropped), so the file plays to its last frame. A
//! process killed mid-job leaves no `.webm`, only the `.part` spool.
//!
//! Without `media-encode` the job is refused at `render.out_start` with `media_encode_unavailable`;
//! the type here then has no value.

use std::path::{Path, PathBuf};

use super::plan::Request;

/// An in-process movie encoder.
pub struct EncodedMovie {
    inner: imp::Inner,
}

impl EncodedMovie {
    /// Starts the encoder thread for `request` (format `webm`).
    pub fn open(request: &Request) -> Result<Self, String> {
        imp::Inner::open(request).map(|inner| Self { inner })
    }

    /// Reserves a queue place (see `Sink::reserve`): `Ok(false)` when `interrupted` fired first.
    pub fn reserve(&mut self, interrupted: &dyn Fn() -> bool) -> Result<bool, String> {
        self.inner.reserve(interrupted)
    }

    /// Gives back an unused reservation.
    pub fn release(&mut self) {
        self.inner.release();
    }

    /// Sends one 8-bit RGBA frame to the reserved place.
    pub fn send(&mut self, rgba: Vec<u8>) -> Result<(), String> {
        self.inner.send(rgba)
    }

    /// The movie file.
    pub fn path(&self) -> PathBuf {
        self.inner.path()
    }

    /// Ends the feed and writes the movie, with the WAV at `audio` (if any) as its Opus track.
    pub fn finish(self, audio: Option<&Path>) -> Result<PathBuf, String> {
        self.inner.finish(audio)
    }
}

#[cfg(feature = "media-encode")]
mod imp {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread::JoinHandle;

    use crossbeam_channel::{Receiver, Sender, unbounded};
    use regia_media::encode::webm::WebmEncoder;

    use super::super::plan::Request;
    use super::super::writer::{Reserved, Slots};

    /// Frames waiting for the encoder thread beyond the one it encodes (as the ffmpeg feeder).
    const QUEUE: usize = 2;

    pub(super) struct Inner {
        feed: Option<Sender<Vec<u8>>>,
        slots: Arc<Slots>,
        worker: Option<JoinHandle<Result<WebmEncoder, String>>>,
        failed: Arc<Mutex<Option<String>>>,
        movie: PathBuf,
    }

    impl Inner {
        pub(super) fn open(request: &Request) -> Result<Self, String> {
            let movie = request.movie_path();
            let encoder = WebmEncoder::create(
                &movie,
                request.width,
                request.height,
                request.fps_num,
                request.fps_den,
                &request.encode,
            )
            .map_err(|e| format!("{}: {}", e.code, e.message))?;
            let (feed, frames) = unbounded::<Vec<u8>>();
            let slots = Slots::new(QUEUE);
            let failed = Arc::new(Mutex::new(None));
            let guard = slots.consumer();
            let worker = {
                let (slots, failed) = (Arc::clone(&slots), Arc::clone(&failed));
                std::thread::Builder::new()
                    .name("render-out-encode".into())
                    .spawn(move || {
                        let _guard = guard;
                        encode_frames(encoder, &frames, &slots, &failed)
                    })
                    .map_err(|e| format!("cannot start the encoder thread: {e}"))?
            };
            Ok(Self {
                feed: Some(feed),
                slots,
                worker: Some(worker),
                failed,
                movie,
            })
        }

        fn stopped(&self) -> String {
            self.failed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
                .unwrap_or_else(|| "the encoder stopped".into())
        }

        pub(super) fn reserve(&mut self, interrupted: &dyn Fn() -> bool) -> Result<bool, String> {
            match self.slots.reserve(interrupted) {
                Reserved::Yes => Ok(true),
                Reserved::Interrupted => Ok(false),
                Reserved::Closed => Err(self.stopped()),
            }
        }

        pub(super) fn release(&mut self) {
            self.slots.release();
        }

        pub(super) fn send(&mut self, rgba: Vec<u8>) -> Result<(), String> {
            let feed = self.feed.as_ref().ok_or("the encoder input is closed")?;
            feed.send(rgba).map_err(|_| self.stopped())
        }

        pub(super) fn path(&self) -> PathBuf {
            self.movie.clone()
        }

        pub(super) fn finish(mut self, audio: Option<&Path>) -> Result<PathBuf, String> {
            // Closing the feed lets the encoder thread finish the queued frames and return. Unlike
            // an `ffmpeg` pipe it never waits on another program, so a stalled close needs no grace.
            self.feed = None;
            let encoder = match self.worker.take().map(JoinHandle::join) {
                Some(Ok(result)) => result?,
                Some(Err(_)) => return Err("the encoder thread panicked".into()),
                None => return Err("the encoder thread is gone".into()),
            };
            let pcm = match audio {
                Some(path) => {
                    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
                    Some(regia_media::encode::wav::parse(&bytes).map_err(|e| e.to_string())?)
                }
                None => None,
            };
            encoder
                .finish(pcm.as_ref())
                .map(|_| self.movie.clone())
                .map_err(|e| e.to_string())
        }
    }

    /// The encoder thread: encodes each frame in order; a failure is kept for the app thread (which
    /// sees the queue close) and ends the thread.
    fn encode_frames(
        mut encoder: WebmEncoder,
        frames: &Receiver<Vec<u8>>,
        slots: &Slots,
        failed: &Mutex<Option<String>>,
    ) -> Result<WebmEncoder, String> {
        while let Ok(rgba) = frames.recv() {
            slots.release();
            let _span = tracing::debug_span!(target: "regia_agent", "render_out_encode_frame").entered();
            if let Err(e) = encoder.push_rgba(&rgba) {
                let error = e.to_string();
                failed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get_or_insert_with(|| error.clone());
                return Err(error);
            }
        }
        Ok(encoder)
    }
}

#[cfg(not(feature = "media-encode"))]
mod imp {
    use std::path::{Path, PathBuf};

    use super::super::plan::Request;

    /// No value exists without the encoder.
    pub(super) enum Inner {}

    impl Inner {
        pub(super) fn open(_request: &Request) -> Result<Self, String> {
            Err(regia_media::encode::EncodeError::unavailable().to_string())
        }

        pub(super) fn reserve(&mut self, _interrupted: &dyn Fn() -> bool) -> Result<bool, String> {
            match *self {}
        }

        pub(super) fn release(&mut self) {
            match *self {}
        }

        pub(super) fn send(&mut self, _rgba: Vec<u8>) -> Result<(), String> {
            match *self {}
        }

        pub(super) fn path(&self) -> PathBuf {
            match *self {}
        }

        pub(super) fn finish(self, _audio: Option<&Path>) -> Result<PathBuf, String> {
            match self {}
        }
    }
}
