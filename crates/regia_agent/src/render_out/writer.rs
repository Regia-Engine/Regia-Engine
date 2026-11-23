// SPDX-License-Identifier: MPL-2.0
//! The frame writer pool (ADR-1412): encoding a frame (PNG deflate, or the raw copy) and writing
//! its file happen on worker threads, never on the render thread.
//!
//! The render thread hands each delivered frame to the pool ([`WriterPool::reserve`], then
//! [`WriterPool::send_reserved`]), which blocks while the
//! queue is full (back-pressure), so memory stays flat: at most `workers` frames wait in the queue
//! and `workers` more are being encoded. The wait is a reservation of a queue slot (`Slots`),
//! taken in short slices so that a cancel request can interrupt it (ADR-2396): the app thread
//! then keeps the frame and lets the request be handled instead of waiting on a stalled disk for
//! good. Workers finish in any order; a frame file is written to
//! `<name>.part` and renamed, so a file under its final name is always complete. The pool tracks
//! the longest run of finished frames from frame 0 (`written`), which is what progress and the
//! manifest report and what [`WriterPool::finish`] keeps: every frame below it exists, nothing
//! at or above it is left on disk.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};

use super::aov::{Aov, AovFormat, AovPlanes};
use super::plan::{Format, PngLevel, Request};

/// The pixels of one delivered frame, tightly packed. `Clone` for a frame two takes of a render
/// job hold (ADR-1616).
#[derive(Clone)]
pub enum FrameData {
    /// 8 bits per channel, RGBA.
    Rgba8(Vec<u8>),
    /// 16 bits per channel (native `u16` codes), RGBA.
    Rgba16(Vec<u16>),
    /// Scene-linear RGB, `f32`, interleaved (format `exr`, ADR-1521).
    RgbF32(Vec<f32>),
}

/// One AOV of a delivered frame, written next to it (ADR-1521).
#[derive(Clone)]
pub struct AovFile {
    /// Which AOV.
    pub aov: Aov,
    /// Its channels at the delivered size.
    pub planes: AovPlanes,
}

struct Task {
    index: u64,
    data: FrameData,
    aovs: Vec<AovFile>,
}

/// What the workers share.
#[derive(Default)]
struct State {
    /// Frames `0..written` are all on disk.
    written: u64,
    /// Finished frames above `written` (their predecessors are still being written).
    ahead: BTreeSet<u64>,
    /// The first failure; once set, workers drop the frames they take.
    error: Option<String>,
}

impl State {
    fn finished(&mut self, index: u64) {
        self.ahead.insert(index);
        while self.ahead.remove(&self.written) {
            self.written += 1;
        }
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How long one wait for a free slot lasts before the waiter checks whether it was interrupted.
/// Only how soon an interruption is seen depends on it, never which frames are made.
const SLOT_WAIT: Duration = Duration::from_millis(50);

/// How long closing waits for a consumer that was stalled when its job ended before it gives up on
/// that consumer (ADR-2396).
pub(crate) const STALLED_CLOSE_GRACE: Duration = Duration::from_secs(10);

#[derive(Default)]
struct SlotState {
    /// Queue places nobody has reserved.
    free: usize,
    /// Consumer threads still running.
    live: usize,
}

/// The places of a frame queue (ADR-2396): the producer reserves one before it sends, a consumer
/// frees one when it takes a frame. The same bound as a bounded channel, but the producer's wait
/// can be interrupted, and it ends (as an error) once every consumer is gone.
pub(crate) struct Slots {
    state: Mutex<SlotState>,
    changed: Condvar,
}

/// What a reservation came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reserved {
    /// A place is held for the next send.
    Yes,
    /// The waiter's interruption fired first; nothing is held.
    Interrupted,
    /// Every consumer is gone; nothing will ever take a frame.
    Closed,
}

impl Slots {
    /// `places` free places and no consumer yet.
    pub(crate) fn new(places: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SlotState {
                free: places,
                live: 0,
            }),
            changed: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts one more consumer; the guard counts it out when its thread ends (or panics).
    pub(crate) fn consumer(self: &Arc<Self>) -> ConsumerGuard {
        self.lock().live += 1;
        ConsumerGuard(Arc::clone(self))
    }

    /// Reserves a place, waiting while none is free. `interrupted` is asked between waits.
    pub(crate) fn reserve(&self, interrupted: &dyn Fn() -> bool) -> Reserved {
        let mut state = self.lock();
        loop {
            if state.live == 0 {
                return Reserved::Closed;
            }
            if state.free > 0 {
                state.free -= 1;
                return Reserved::Yes;
            }
            if interrupted() {
                return Reserved::Interrupted;
            }
            state = self
                .changed
                .wait_timeout(state, SLOT_WAIT)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Frees one place (a consumer took a frame, or a reservation is given back unused).
    pub(crate) fn release(&self) {
        self.lock().free += 1;
        self.changed.notify_all();
    }

    /// Waits up to `grace` for every consumer to end; `true` when they all did.
    pub(crate) fn wait_idle(&self, grace: Duration) -> bool {
        let state = self.lock();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, grace, |state| state.live > 0)
            .unwrap_or_else(PoisonError::into_inner);
        state.live == 0
    }
}

/// One consumer thread of a [`Slots`] queue; dropping it counts the consumer out.
pub(crate) struct ConsumerGuard(Arc<Slots>);

impl Drop for ConsumerGuard {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.live = state.live.saturating_sub(1);
        drop(state);
        self.0.changed.notify_all();
    }
}

/// What a closed pool leaves behind.
pub struct Closed {
    /// Frames `0..written` are on disk and complete.
    pub written: u64,
    /// The first write failure, if any.
    pub error: Option<String>,
}

/// A bounded pool of frame-writing threads for one job.
pub struct WriterPool {
    request: Arc<Request>,
    queue: Option<Sender<Task>>,
    slots: Arc<Slots>,
    threads: Vec<JoinHandle<()>>,
    state: Arc<Mutex<State>>,
    /// Set while the last hand-over was interrupted with the queue full (ADR-2396): closing then
    /// waits only `STALLED_CLOSE_GRACE` for the writers.
    stalled: bool,
}

impl WriterPool {
    /// Starts `workers` threads (at least one) with a queue of the same length.
    pub fn new(request: &Request, workers: usize) -> Self {
        let workers = workers.max(1);
        let request = Arc::new(request.clone());
        let state = Arc::new(Mutex::new(State::default()));
        let (queue, receiver) = unbounded::<Task>();
        let slots = Slots::new(workers);
        let threads = (0..workers)
            .filter_map(|n| {
                let (request, state, receiver) = (request.clone(), state.clone(), receiver.clone());
                let guard = slots.consumer();
                let slots = Arc::clone(&slots);
                // A thread that does not start drops its guard here, which counts it out.
                std::thread::Builder::new()
                    .name(format!("render-out-writer-{n}"))
                    .spawn(move || {
                        let _guard = guard;
                        work(&request, &state, &receiver, &slots);
                    })
                    .ok()
            })
            .collect::<Vec<_>>();
        if threads.is_empty() {
            lock(&state).error = Some("cannot start a frame writer thread".into());
        }
        Self {
            request,
            queue: Some(queue),
            slots,
            threads,
            state,
            stalled: false,
        }
    }

    /// Reserves a queue place for the next [`Self::send_reserved`], waiting while the queue is
    /// full (back-pressure); `Ok(false)` when `interrupted` fired first (nothing is held then).
    /// Fails with the first write error seen so far, so a failing disk stops the job within a
    /// frame or two.
    pub(crate) fn reserve(&self, interrupted: &dyn Fn() -> bool) -> Result<bool, String> {
        self.check()?;
        match self.slots.reserve(interrupted) {
            Reserved::Yes => Ok(true),
            Reserved::Interrupted => Ok(false),
            Reserved::Closed => Err(self.stopped()),
        }
    }

    /// Gives back a place reserved by [`Self::reserve`] and not used.
    pub(crate) fn release_reserved(&self) {
        self.slots.release();
    }

    /// Queues frame `index` into the place [`Self::reserve`] held (never waits). The frame counts
    /// as written only when the frame and every one of its AOV files are on disk.
    pub(crate) fn send_reserved(
        &self,
        index: u64,
        data: FrameData,
        aovs: Vec<AovFile>,
    ) -> Result<(), String> {
        if let Err(error) = self.check() {
            self.slots.release();
            return Err(error);
        }
        let queue = self.queue.as_ref().ok_or("the output is closed")?;
        queue.send(Task { index, data, aovs }).map_err(|_| self.stopped())
    }

    /// Notes whether the last hand-over was interrupted with the queue full.
    pub(crate) fn set_stalled(&mut self, stalled: bool) {
        self.stalled = stalled;
    }

    fn check(&self) -> Result<(), String> {
        match lock(&self.state).error.clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn stopped(&self) -> String {
        lock(&self.state)
            .error
            .clone()
            .unwrap_or_else(|| "the frame writers stopped".into())
    }

    /// Frames `0..written()` are on disk.
    pub fn written(&self) -> u64 {
        lock(&self.state).written
    }

    /// The request the pool writes for.
    pub fn request(&self) -> &Request {
        &self.request
    }

    /// Waits for every queued frame, joins the threads, and removes any frame file above the
    /// unbroken run from frame 0 (only a failure leaves such files).
    pub fn finish(mut self) -> Closed {
        self.close()
    }

    fn close(&mut self) -> Closed {
        self.queue = None;
        // A pool whose writers were stalled when the job ended (a hung disk) is not waited on for
        // good: after a grace the writers still busy are left to finish on their own (ADR-2396).
        let left_running = self.stalled && !self.slots.wait_idle(STALLED_CLOSE_GRACE);
        let mut panicked = false;
        for thread in self.threads.drain(..) {
            if left_running && !thread.is_finished() {
                continue;
            }
            panicked |= thread.join().is_err();
        }
        let mut state = lock(&self.state);
        if panicked && state.error.is_none() {
            state.error = Some("a frame writer thread panicked".into());
        }
        if left_running && state.error.is_none() {
            state.error = Some(
                "the frame writers were still busy after the job ended (stalled disk); frames they had not finished are not kept"
                    .into(),
            );
        }
        let written = state.written;
        for index in std::mem::take(&mut state.ahead) {
            for path in self.request.frame_files(index) {
                let _ = std::fs::remove_file(path);
            }
        }
        Closed {
            written,
            error: state.error.clone(),
        }
    }
}

impl Drop for WriterPool {
    fn drop(&mut self) {
        self.close();
    }
}

fn work(request: &Request, state: &Mutex<State>, receiver: &Receiver<Task>, slots: &Slots) {
    loop {
        let Ok(Task { index, data, aovs }) = receiver.recv() else {
            return;
        };
        // The frame left the queue: its place is free again.
        slots.release();
        if lock(state).error.is_some() {
            continue;
        }
        let _span = tracing::debug_span!(target: "regia_agent", "render_out_write", index).entered();
        let result = aovs
            .iter()
            .try_for_each(|file| {
                write_aov(
                    &request
                        .aovs
                        .path(&request.out_dir, &request.name, file.aov, index),
                    file,
                    request.width,
                    request.height,
                    request.aovs.format,
                )
            })
            .and_then(|()| {
                write_frame(
                    &request.frame_path(index),
                    &data,
                    request.width,
                    request.height,
                    request.format,
                    request.png_level,
                )
            });
        if result.is_err() {
            // A frame is all of its files or none of them.
            for path in request.frame_files(index) {
                let _ = std::fs::remove_file(path);
            }
        }
        let mut state = lock(state);
        match result {
            Ok(()) => state.finished(index),
            Err(error) => {
                state.error.get_or_insert(error);
            }
        }
    }
}

/// The path a file is written to before it is renamed into place.
fn part_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

/// Writes one frame file (`format` is [`Format::Png`], [`Format::Raw`] or [`Format::Exr`])
/// atomically.
pub fn write_frame(
    path: &Path,
    data: &FrameData,
    width: u32,
    height: u32,
    format: Format,
    level: PngLevel,
) -> Result<(), String> {
    let part = part_path(path);
    let written = match (format, data) {
        (Format::Exr, FrameData::RgbF32(rgb)) => super::exr_file::write_planes(
            &part,
            width,
            height,
            &super::exr_file::RGB_CHANNELS,
            &super::exr_file::rgb_planes(rgb),
        ),
        (_, FrameData::RgbF32(_)) | (Format::Exr, _) => Err(format!(
            "{}: scene-linear frames are written as EXR only",
            path.display()
        )),
        (Format::Raw, FrameData::Rgba8(rgba)) => write_raw(&part, rgba),
        (Format::Raw, FrameData::Rgba16(rgba16)) => write_raw(&part, &raw16_bytes(rgba16)),
        (_, FrameData::Rgba8(rgba)) => write_png(&part, rgba, width, height, level),
        (_, FrameData::Rgba16(rgba16)) => write_png16(&part, rgba16, width, height, level),
    };
    finish_part(&part, path, written)
}

/// Renames a finished `part` file into place, or removes it after a failed write.
fn finish_part(part: &Path, path: &Path, written: Result<(), String>) -> Result<(), String> {
    let result =
        written.and_then(|()| std::fs::rename(part, path).map_err(|e| format!("{}: {e}", path.display())));
    if result.is_err() {
        let _ = std::fs::remove_file(part);
    }
    result
}

/// Writes one AOV file atomically: an EXR with the AOV's channels, or the raw interleaved bytes.
pub fn write_aov(
    path: &Path,
    file: &AovFile,
    width: u32,
    height: u32,
    format: AovFormat,
) -> Result<(), String> {
    let part = part_path(path);
    let written = match format {
        AovFormat::Exr => {
            super::exr_file::write_planes(&part, width, height, file.aov.channels(), &file.planes)
        }
        AovFormat::Raw => write_raw(&part, &file.planes.interleaved_le()),
        AovFormat::Png | AovFormat::Png16 => super::aov_png::write_png_aov(
            &part,
            width,
            height,
            file.aov,
            &file.planes,
            format == AovFormat::Png16,
        ),
    };
    finish_part(&part, path, written)
}

/// The bytes of a raw 16-bit frame: each code little-endian.
pub fn raw16_bytes(rgba16: &[u16]) -> Vec<u8> {
    rgba16.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn write_raw(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn compression(level: PngLevel) -> png::Compression {
    match level {
        PngLevel::Fast => png::Compression::Fast,
        PngLevel::Balanced => png::Compression::Balanced,
        PngLevel::Best => png::Compression::High,
    }
}

/// Writes `rgba` as an 8-bit RGBA PNG. The encoder is deterministic: the same pixels and level
/// give the same file.
pub fn write_png(path: &Path, rgba: &[u8], width: u32, height: u32, level: PngLevel) -> Result<(), String> {
    encode_png(path, rgba, width, height, png::BitDepth::Eight, level)
}

/// Writes `rgba16` (RGBA, 16-bit codes) as a 16-bit RGBA PNG (big-endian samples, as PNG stores
/// them). Deterministic like [`write_png`].
pub fn write_png16(
    path: &Path,
    rgba16: &[u16],
    width: u32,
    height: u32,
    level: PngLevel,
) -> Result<(), String> {
    let bytes: Vec<u8> = rgba16.iter().flat_map(|v| v.to_be_bytes()).collect();
    encode_png(path, &bytes, width, height, png::BitDepth::Sixteen, level)
}

fn encode_png(
    path: &Path,
    bytes: &[u8],
    width: u32,
    height: u32,
    depth: png::BitDepth,
    level: PngLevel,
) -> Result<(), String> {
    let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::with_capacity(1 << 20, file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(depth);
    encoder.set_compression(compression(level));
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writer
        .write_image_data(bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writer.finish().map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    impl WriterPool {
        /// Queues frame `index`, waiting while the queue is full.
        fn submit(&self, index: u64, data: FrameData) -> Result<(), String> {
            self.submit_with_aovs(index, data, Vec::new())
        }

        /// [`Self::submit`] with the frame's AOV files.
        fn submit_with_aovs(&self, index: u64, data: FrameData, aovs: Vec<AovFile>) -> Result<(), String> {
            if self.reserve(&|| false)? {
                self.send_reserved(index, data, aovs)
            } else {
                Err("not handed over".into())
            }
        }
    }
    use serde_json::json;

    fn request(dir: &Path, format: &str, frames: u64) -> Request {
        Request::parse(&json!({
            "out": dir.display().to_string(), "frames": frames, "width": 4, "height": 3, "format": format
        }))
        .expect("request")
    }

    /// A frame whose pixels depend on its index.
    fn frame(index: u64) -> Vec<u8> {
        (0..4 * 3 * 4)
            .map(|i| (i as u64 * 7 + index * 31) as u8)
            .collect()
    }

    fn decode(path: &Path) -> Vec<u8> {
        let decoder = png::Decoder::new(std::io::BufReader::new(File::open(path).expect("open")));
        let mut reader = decoder.read_info().expect("header");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        let info = reader.next_frame(&mut buf).expect("frame");
        buf.truncate(info.buffer_size());
        buf
    }

    #[test]
    fn every_submitted_frame_is_written_in_order_with_any_worker_count() {
        for workers in [1, 2, 5] {
            let dir = regia_test_utils::unique_temp_dir("render-out-pool");
            let r = request(&dir, "png", 20);
            let pool = WriterPool::new(&r, workers);
            for index in 0..20 {
                pool.submit(index, FrameData::Rgba8(frame(index)))
                    .expect("submit");
            }
            let closed = pool.finish();
            assert_eq!((closed.written, closed.error), (20, None), "{workers} workers");
            for index in 0..20 {
                assert_eq!(decode(&r.frame_path(index)), frame(index), "frame {index}");
            }
            let leftovers = std::fs::read_dir(&dir)
                .expect("dir")
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "part"))
                .count();
            assert_eq!(leftovers, 0, "no half-written files");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_failed_frame_keeps_only_the_unbroken_run_from_frame_zero() {
        let dir = regia_test_utils::unique_temp_dir("render-out-pool-fail");
        let r = request(&dir, "raw", 6);
        // Frame 3's name is taken by a directory, so writing it fails; one worker keeps the
        // order fixed so frames 4 and 5 are never reached after the failure.
        std::fs::create_dir_all(r.frame_path(3)).expect("blocker");
        let pool = WriterPool::new(&r, 1);
        for index in 0..6 {
            // Later submits may be refused once the failure is known; that is fine.
            let _ = pool.submit(index, FrameData::Rgba8(frame(index)));
        }
        let closed = pool.finish();
        assert_eq!(closed.written, 3);
        assert!(closed.error.is_some());
        for index in 0..3 {
            assert!(r.frame_path(index).is_file(), "frame {index}");
        }
        for index in 4..6 {
            assert!(!r.frame_path(index).exists(), "frame {index} is not left behind");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_order_completions_advance_the_unbroken_run_only_when_the_gap_fills() {
        let mut state = State::default();
        state.finished(2);
        state.finished(1);
        assert_eq!(state.written, 0);
        state.finished(0);
        assert_eq!(state.written, 3);
        assert!(state.ahead.is_empty());
        state.finished(4);
        assert_eq!((state.written, state.ahead.len()), (3, 1));
    }

    #[test]
    fn png_levels_and_raw_decode_to_the_same_pixels() {
        let dir = regia_test_utils::unique_temp_dir("render-out-levels");
        let pixels = frame(5);
        let mut sizes = Vec::new();
        for level in [PngLevel::Fast, PngLevel::Balanced, PngLevel::Best] {
            let path = dir.join(format!("{}.png", level.name()));
            write_frame(&path, &FrameData::Rgba8(pixels.clone()), 4, 3, Format::Png, level).expect("png");
            assert_eq!(decode(&path), pixels, "{}", level.name());
            sizes.push(std::fs::metadata(&path).expect("meta").len());
        }
        assert!(sizes.iter().all(|s| *s > 0));
        let raw = dir.join("frame.raw");
        write_frame(
            &raw,
            &FrameData::Rgba8(pixels.clone()),
            4,
            3,
            Format::Raw,
            PngLevel::Fast,
        )
        .expect("raw");
        assert_eq!(std::fs::read(&raw).expect("read"), pixels);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn raw_16_bit_is_little_endian_rgba() {
        let dir = regia_test_utils::unique_temp_dir("render-out-raw16");
        let codes = [0u16, 1, 0x1234, 65535, 40_000, 257, 65534, 0x8000];
        let raw = dir.join("frame.raw");
        write_frame(
            &raw,
            &FrameData::Rgba16(codes.to_vec()),
            2,
            1,
            Format::Raw,
            PngLevel::Fast,
        )
        .expect("raw");
        let bytes = std::fs::read(&raw).expect("read");
        assert_eq!(bytes.len(), 2 * 4 * 2);
        assert_eq!(&bytes[4..6], &[0x34, 0x12]);
        let (pairs, _) = bytes.as_chunks::<2>();
        let back: Vec<u16> = pairs.iter().map(|b| u16::from_le_bytes(*b)).collect();
        assert_eq!(back, codes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_frame_with_aovs_is_written_whole_and_counted_once() {
        let dir = regia_test_utils::unique_temp_dir("render-out-pool-aov");
        let r = Request::parse(&json!({
            "out": dir.display().to_string(), "frames": 2, "width": 2, "height": 1,
            "format": "raw", "aovs": ["depth", "id"], "aov_format": "raw"
        }))
        .expect("request");
        let pool = WriterPool::new(&r, 2);
        for index in 0..2 {
            let aovs = vec![
                AovFile {
                    aov: Aov::Depth,
                    planes: AovPlanes::F32(vec![vec![1.0 + index as f32, f32::INFINITY]]),
                },
                AovFile {
                    aov: Aov::Id,
                    planes: AovPlanes::U32(vec![vec![1, 0], vec![7, 0]]),
                },
            ];
            pool.submit_with_aovs(index, FrameData::Rgba8(vec![9; 8]), aovs)
                .expect("submit");
        }
        let closed = pool.finish();
        assert_eq!((closed.written, closed.error), (2, None));
        for index in 0..2 {
            for path in r.frame_files(index) {
                assert!(path.is_file(), "{}", path.display());
            }
        }
        let depth = std::fs::read(r.aovs.path(&r.out_dir, &r.name, Aov::Depth, 1)).expect("depth");
        assert_eq!(&depth[..4], &2.0f32.to_le_bytes());
        let ids = std::fs::read(r.aovs.path(&r.out_dir, &r.name, Aov::Id, 0)).expect("ids");
        assert_eq!(ids, [1, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_aov_leaves_no_file_of_its_frame() {
        let dir = regia_test_utils::unique_temp_dir("render-out-pool-aov-fail");
        let r = Request::parse(&json!({
            "out": dir.display().to_string(), "frames": 2, "width": 2, "height": 1,
            "aovs": ["depth", "id"]
        }))
        .expect("request");
        // Frame 1's id file name is taken by a directory: the depth file of frame 1 is written
        // first, then the id write fails, and frame 1 must not keep its depth file or its PNG.
        std::fs::create_dir_all(r.aovs.path(&r.out_dir, &r.name, Aov::Id, 1)).expect("blocker");
        let pool = WriterPool::new(&r, 1);
        for index in 0..2 {
            let aovs = vec![
                AovFile {
                    aov: Aov::Depth,
                    planes: AovPlanes::F32(vec![vec![1.0, 2.0]]),
                },
                AovFile {
                    aov: Aov::Id,
                    planes: AovPlanes::U32(vec![vec![1, 2], vec![1, 1]]),
                },
            ];
            let _ = pool.submit_with_aovs(index, FrameData::Rgba8(vec![9; 8]), aovs);
        }
        let closed = pool.finish();
        assert_eq!(closed.written, 1);
        assert!(closed.error.is_some());
        assert!(r.frame_files(0).iter().all(|p| p.is_file()));
        assert!(!r.aovs.path(&r.out_dir, &r.name, Aov::Depth, 1).exists());
        assert!(!r.frame_path(1).exists());
        let leftovers = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "part"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scene_linear_frames_go_to_exr_only() {
        let dir = regia_test_utils::unique_temp_dir("render-out-exr-frame");
        let rgb = vec![0.5, 1.0e4, 2.0, 0.0, 0.25, 65_536.0];
        let exr = dir.join("frame.exr");
        write_frame(
            &exr,
            &FrameData::RgbF32(rgb.clone()),
            2,
            1,
            Format::Exr,
            PngLevel::Fast,
        )
        .expect("exr");
        assert!(exr.is_file());
        let png = dir.join("frame.png");
        assert!(write_frame(&png, &FrameData::RgbF32(rgb), 2, 1, Format::Png, PngLevel::Fast).is_err());
        assert!(
            write_frame(
                &exr,
                &FrameData::Rgba8(vec![0; 8]),
                2,
                1,
                Format::Exr,
                PngLevel::Fast
            )
            .is_err()
        );
        assert!(!png.exists() && !dir.join("frame.png.part").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A gradient with a little noise: compresses like a rendered frame, not like a flat colour
    /// or pure noise.
    fn synthetic_frame(width: u32, height: u32) -> Vec<u8> {
        let mut seed = 0x2545_f491u32;
        (0..width * height)
            .flat_map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 29) as u8;
                let x = (i % width) as u8;
                let y = (i / width) as u8;
                [x.wrapping_add(noise), y.wrapping_add(noise), x ^ y, 255]
            })
            .collect()
    }

    /// Microbenchmark, never run by the gate (`docs/06-testing-and-ci.md` § 9): frames per second
    /// of the writer pool on a synthetic 4K frame source (no GPU), against the old path (one
    /// thread, the `png` crate's balanced level, on the calling thread). Prints a table.
    #[test]
    #[ignore = "microbenchmark: run by hand, see the ADR-1412 lane report"]
    fn perf_micro_writer_pool_4k() {
        use std::time::Instant;
        const FRAMES: u64 = 24;
        let (width, height) = (3840u32, 2160u32);
        let frame = synthetic_frame(width, height);
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        println!(
            "perf-micro cores={cores} frame={width}x{height} bytes={}",
            frame.len()
        );
        let dir = regia_test_utils::unique_temp_dir("render-out-perf");
        // The old path: encode on the calling thread, balanced, one frame at a time.
        let start = Instant::now();
        for index in 0..4u64 {
            write_png(
                &dir.join(format!("old_{index}.png")),
                &frame,
                width,
                height,
                PngLevel::Balanced,
            )
            .expect("old");
        }
        println!(
            "perf-micro old serial balanced: {:.3} s/frame",
            start.elapsed().as_secs_f64() / 4.0
        );
        for (label, format, level, workers) in [
            ("png fast 1 writer", "png", "fast", 1usize),
            ("png fast auto", "png", "fast", 0),
            ("png balanced auto", "png", "balanced", 0),
            ("png best auto", "png", "best", 0),
            ("raw auto", "raw", "fast", 0),
        ] {
            let sub = dir.join(label.replace(' ', "_"));
            std::fs::create_dir_all(&sub).expect("dir");
            let r = Request::parse(&json!({
                "out": sub.display().to_string(), "frames": FRAMES, "width": width, "height": height,
                "format": format, "png_compression": level, "writers": workers
            }))
            .expect("request");
            let threads = r.writer_threads();
            let pool = WriterPool::new(&r, threads);
            let start = Instant::now();
            for index in 0..FRAMES {
                pool.submit(index, FrameData::Rgba8(frame.clone()))
                    .expect("submit");
            }
            let closed = pool.finish();
            let seconds = start.elapsed().as_secs_f64();
            assert_eq!((closed.written, closed.error), (FRAMES, None));
            let size = std::fs::metadata(r.frame_path(0)).expect("meta").len();
            println!(
                "perf-micro {label} ({threads} writers): {:.3} s/frame, {:.1} MB/file",
                seconds / FRAMES as f64,
                size as f64 / 1e6
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
