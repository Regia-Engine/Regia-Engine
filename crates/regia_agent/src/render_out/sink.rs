// SPDX-License-Identifier: MPL-2.0
//! Where render-out frames go: a PNG or raw file per frame (written by a worker pool, ADR-1412),
//! an `ffmpeg` process fed raw RGBA on stdin, or the in-process WebM encoder (format `webm`,
//! `super::encode_sink`, ADR-3369).
//!
//! The video path never links an encoder: `ffmpeg` is a separate program found on `PATH` and
//! talked to over a pipe (ADR-267), so no GPL code enters the engine even when the user's build
//! of it is GPL. Frames reach it in order, one `write_all` each, from a feeder thread (ADR-2396);
//! closing stdin ends the movie.

use std::io::{BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, unbounded};

use super::encode_sink::EncodedMovie;
use super::plan::{Format, Request};
use super::writer::{AovFile, FrameData, Reserved, STALLED_CLOSE_GRACE, Slots, WriterPool};

/// Where `ffmpeg` is on `PATH`, if anywhere.
pub fn find_ffmpeg() -> Option<PathBuf> {
    find_on_path("ffmpeg", regia_core::brand::env_var_os("PATH").as_deref())
}

/// The first executable file called `program` (plus `.exe` on Windows) in `path`'s directories.
pub fn find_on_path(program: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path = path?;
    let file = if cfg!(windows) {
        format!("{program}.exe")
    } else {
        program.to_string()
    };
    std::env::split_paths(path)
        .map(|dir| dir.join(&file))
        .find(|candidate| candidate.is_file())
}

/// The `ffmpeg` arguments (after the program name) that turn raw RGBA frames on stdin into the
/// request's movie. Pure, so the command is tested without `ffmpeg` installed.
pub fn ffmpeg_args(request: &Request) -> Vec<String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgba",
        "-s",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    args.push(format!("{}x{}", request.width, request.height));
    args.push("-framerate".into());
    args.push(format!("{}/{}", request.fps_num, request.fps_den));
    args.extend(["-i", "-"].iter().map(|s| (*s).to_string()));
    let codec: &[&str] = match request.format {
        // Lossless, intra-only (every frame a keyframe, so any frame can be cut to), with slice
        // CRCs so a damaged file is detected. The content is sRGB; the RGB stays RGB.
        Format::Ffv1 => &[
            "-c:v",
            "ffv1",
            "-level",
            "3",
            "-coder",
            "1",
            "-context",
            "1",
            "-g",
            "1",
            "-slices",
            "16",
            "-slicecrc",
            "1",
            "-pix_fmt",
            "bgr0",
        ],
        // 4444 at 10 bit with full-resolution chroma; Rec. 709 matrix, sRGB transfer tag.
        Format::ProRes => &[
            "-vf",
            "scale=out_color_matrix=bt709",
            "-c:v",
            "prores_ks",
            "-profile:v",
            "4444",
            "-vendor",
            "apl0",
            "-pix_fmt",
            "yuv444p10le",
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "iec61966-2-1",
        ],
        Format::DnxHr => &[
            "-vf",
            "scale=out_color_matrix=bt709",
            "-c:v",
            "dnxhd",
            "-profile:v",
            "dnxhr_444",
            "-pix_fmt",
            "yuv444p10le",
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "iec61966-2-1",
        ],
        Format::Png | Format::Raw | Format::Exr | Format::Webm => &[],
    };
    args.extend(codec.iter().map(|s| (*s).to_string()));
    args.push(request.movie_path().display().to_string());
    args
}

/// The frame consumer of one job.
pub enum Sink {
    /// One file per frame (PNG or raw), encoded and written by a bounded pool of worker threads.
    Frames(WriterPool),
    /// An `ffmpeg` child fed on stdin by its own thread.
    Ffmpeg(Movie),
    /// The in-process AV1/Opus WebM encoder on its own thread (format `webm`, ADR-3369).
    Encoded(EncodedMovie),
}

/// Frames waiting for the feeder thread, beyond the one it is writing: the same bound as a frame
/// pool of two writers, so memory stays flat when ffmpeg is slower than the renderer.
const FEED_QUEUE: usize = 2;

/// ffmpeg's error output kept for the close-time message (it runs with `-loglevel error`, so this
/// is a cap against a runaway encoder, not the normal size).
const MAX_FFMPEG_LOG: usize = 64 * 1024;

/// An `ffmpeg` child and the threads that talk to it (ADR-2396). The app thread never touches
/// the pipes: a feeder thread writes the frames to stdin in order (one `write_all` each) and a
/// drain thread keeps reading stderr, so neither a slow encoder nor a full error pipe can park the
/// app thread inside a write.
pub struct Movie {
    child: Child,
    /// The frames for the feeder; `None` once closed (closing stdin ends the movie).
    feed: Option<Sender<Vec<u8>>>,
    slots: Arc<Slots>,
    feeder: Option<JoinHandle<Result<(), String>>>,
    log: Option<JoinHandle<Vec<u8>>>,
    /// The feeder's write error, once it stopped.
    failed: Arc<Mutex<Option<String>>>,
    /// The movie being written.
    movie: PathBuf,
    /// Set while the last hand-over was interrupted with the feed full: closing then waits only
    /// `STALLED_CLOSE_GRACE` for ffmpeg to take the rest before it stops the process.
    stalled: bool,
}

/// What a closed sink leaves behind.
pub struct Finished {
    /// The files produced: frames `0..n` of a frame-file sink, or the movie.
    pub files: Vec<PathBuf>,
    /// For a frame-file sink, the number of frames on disk (the unbroken run from frame 0).
    pub frames: Option<u64>,
    /// What went wrong while writing, if anything.
    pub error: Option<String>,
}

impl Sink {
    /// Creates the output directory and opens the sink for `request`.
    pub fn open(request: &Request) -> Result<Self, String> {
        std::fs::create_dir_all(&request.out_dir)
            .map_err(|e| format!("{}: {e}", request.out_dir.display()))?;
        if request.format.is_frame_files() {
            return Ok(Self::Frames(WriterPool::new(request, request.writer_threads())));
        }
        if request.format.is_encoded() {
            return EncodedMovie::open(request).map(Self::Encoded);
        }
        let program = find_ffmpeg().ok_or_else(|| {
            "no `ffmpeg` on PATH (formats ffv1, prores and dnxhr pipe frames to it; use format \"png\" or \"raw\" and assemble later)"
                .to_string()
        })?;
        Self::open_movie(request, &program)
    }

    /// Opens a movie sink that pipes to `program` (the `ffmpeg` found on `PATH`; tests pass a
    /// stand-in).
    pub(super) fn open_movie(request: &Request, program: &std::path::Path) -> Result<Self, String> {
        let mut child = Command::new(program)
            .args(ffmpeg_args(request))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start ffmpeg: {e}"))?;
        let stdin = child.stdin.take().ok_or("ffmpeg has no stdin");
        let stderr = child.stderr.take().ok_or("ffmpeg has no stderr");
        let (stdin, stderr) = match (stdin, stderr) {
            (Ok(stdin), Ok(stderr)) => (stdin, stderr),
            (Err(e), _) | (_, Err(e)) => {
                stop(&mut child);
                return Err(e.into());
            }
        };
        let log = std::thread::Builder::new()
            .name("render-out-ffmpeg-log".into())
            .spawn(move || drain_log(stderr));
        let log = match log {
            Ok(log) => log,
            Err(e) => {
                stop(&mut child);
                return Err(format!("cannot start the ffmpeg log thread: {e}"));
            }
        };
        let (feed, frames) = unbounded::<Vec<u8>>();
        let slots = Slots::new(FEED_QUEUE);
        let failed = Arc::new(Mutex::new(None));
        let guard = slots.consumer();
        let feeder = {
            let (slots, failed) = (Arc::clone(&slots), Arc::clone(&failed));
            std::thread::Builder::new()
                .name("render-out-ffmpeg-feed".into())
                .spawn(move || {
                    let _guard = guard;
                    feed_ffmpeg(BufWriter::with_capacity(1 << 20, stdin), &frames, &slots, &failed)
                })
        };
        let feeder = match feeder {
            Ok(feeder) => feeder,
            Err(e) => {
                stop(&mut child);
                let _ = log.join();
                return Err(format!("cannot start the ffmpeg feed thread: {e}"));
            }
        };
        Ok(Self::Ffmpeg(Movie {
            child,
            feed: Some(feed),
            slots,
            feeder: Some(feeder),
            log: Some(log),
            failed,
            movie: request.movie_path(),
            stalled: false,
        }))
    }

    /// Hands over delivered frame `index` (any frame-file form with its AOV files, ADR-1521; a
    /// movie takes only an 8-bit frame without AOVs), waiting while the queue is full. The wait
    /// ends when `interrupted` says so: the frame comes back (`Ok(Some(..))`) and nothing was
    /// handed over (ADR-2396).
    pub fn hand_over(
        &mut self,
        index: u64,
        data: FrameData,
        aovs: Vec<AovFile>,
        interrupted: &dyn Fn() -> bool,
    ) -> Result<Option<(FrameData, Vec<AovFile>)>, String> {
        if !self.reserve(interrupted)? {
            return Ok(Some((data, aovs)));
        }
        self.send_reserved(index, data, aovs).map(|()| None)
    }

    /// Reserves a queue place for the next [`Self::send_reserved`], waiting while the queue is
    /// full; `Ok(false)` (nothing held, the sink noted as stalled) when `interrupted` fired first.
    pub fn reserve(&mut self, interrupted: &dyn Fn() -> bool) -> Result<bool, String> {
        let reserved = match self {
            Self::Frames(pool) => pool.reserve(interrupted)?,
            Self::Ffmpeg(movie) => match movie.slots.reserve(interrupted) {
                Reserved::Yes => true,
                Reserved::Interrupted => false,
                Reserved::Closed => return Err(movie.stopped()),
            },
            Self::Encoded(movie) => movie.reserve(interrupted)?,
        };
        self.set_stalled(!reserved);
        Ok(reserved)
    }

    /// Gives back a place reserved by [`Self::reserve`] and not used.
    pub fn release_reserved(&mut self) {
        match self {
            Self::Frames(pool) => pool.release_reserved(),
            Self::Ffmpeg(movie) => movie.slots.release(),
            Self::Encoded(movie) => movie.release(),
        }
    }

    /// Hands frame `index` to the place [`Self::reserve`] held; never waits.
    pub fn send_reserved(&mut self, index: u64, data: FrameData, aovs: Vec<AovFile>) -> Result<(), String> {
        match self {
            Self::Frames(pool) => pool.send_reserved(index, data, aovs),
            Self::Ffmpeg(movie) => {
                let rgba = match data {
                    FrameData::Rgba8(rgba) if aovs.is_empty() => rgba,
                    _ => {
                        movie.slots.release();
                        return Err("movies take 8-bit frames without AOVs".into());
                    }
                };
                let feed = movie.feed.as_ref().ok_or("ffmpeg input is closed")?;
                feed.send(rgba).map_err(|_| movie.stopped())
            }
            Self::Encoded(movie) => match data {
                FrameData::Rgba8(rgba) if aovs.is_empty() => movie.send(rgba),
                _ => {
                    movie.release();
                    Err("movies take 8-bit frames without AOVs".into())
                }
            },
        }
    }

    fn set_stalled(&mut self, stalled: bool) {
        match self {
            Self::Frames(pool) => pool.set_stalled(stalled),
            Self::Ffmpeg(movie) => movie.stalled = stalled,
            // The encoder never waits on another program; a stalled close needs no grace.
            Self::Encoded(_) => {}
        }
    }

    /// Frames known to be on disk from frame 0 on without a gap (frame-file sinks only).
    pub fn written(&self) -> Option<u64> {
        match self {
            Self::Frames(pool) => Some(pool.written()),
            Self::Ffmpeg(_) | Self::Encoded(_) => None,
        }
    }

    /// Closes the sink. A frame-file sink first writes everything already handed over (a cancel
    /// loses no delivered frame), then keeps the unbroken run from frame 0 and removes anything
    /// above a gap. A movie is finalized with what was sent (after a cancel or an error too), so
    /// the partial file plays. A sink that was stalled when its job ended is waited on only
    /// `STALLED_CLOSE_GRACE`: a movie's ffmpeg is then stopped and the job fails with what it said.
    pub fn finish(self) -> Finished {
        self.finish_with_audio(None)
    }

    /// True for a sink that takes the job's WAV mixdown into its movie ([`Self::finish_with_audio`]):
    /// format `webm`.
    pub fn takes_audio(&self) -> bool {
        matches!(self, Self::Encoded(_))
    }

    /// [`Self::finish`], with the closed WAV mixdown at `audio` muxed into the movie by a sink that
    /// [`Self::takes_audio`] (others ignore it).
    pub fn finish_with_audio(self, audio: Option<&std::path::Path>) -> Finished {
        match self {
            Self::Encoded(movie) => {
                let path = movie.path();
                match movie.finish(audio) {
                    Ok(path) => Finished {
                        files: vec![path],
                        frames: None,
                        error: None,
                    },
                    Err(error) => Finished {
                        files: Vec::new(),
                        frames: None,
                        error: Some(format!("{}: {error}", path.display())),
                    },
                }
            }
            Self::Frames(pool) => {
                let request = pool.request().clone();
                let closed = pool.finish();
                Finished {
                    files: (0..closed.written).map(|i| request.frame_path(i)).collect(),
                    frames: Some(closed.written),
                    error: closed.error,
                }
            }
            Self::Ffmpeg(movie) => {
                let path = movie.movie.clone();
                match movie.close() {
                    Ok(()) => Finished {
                        files: vec![path],
                        frames: None,
                        error: None,
                    },
                    Err(error) => Finished {
                        files: Vec::new(),
                        frames: None,
                        error: Some(error),
                    },
                }
            }
        }
    }
}

impl Movie {
    fn stopped(&self) -> String {
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| "ffmpeg stopped reading".into())
    }

    /// Ends the feed (the feeder writes what is queued, flushes and closes stdin), then waits for
    /// ffmpeg to finish the movie.
    fn close(mut self) -> Result<(), String> {
        self.feed = None;
        let mut stopped = false;
        if self.stalled && !self.slots.wait_idle(STALLED_CLOSE_GRACE) {
            // The feeder is still inside a write ffmpeg does not take: stop ffmpeg, which ends
            // that write with an error.
            stop(&mut self.child);
            stopped = true;
        }
        let fed = match self.feeder.take().map(JoinHandle::join) {
            Some(Ok(fed)) => fed,
            Some(Err(_)) => Err("the ffmpeg feed thread panicked".into()),
            None => Ok(()),
        };
        let status = self.child.wait().map_err(|e| format!("waiting for ffmpeg: {e}"));
        let log = self
            .log
            .take()
            .and_then(|log| log.join().ok())
            .unwrap_or_default();
        let log = String::from_utf8_lossy(&log);
        if stopped {
            return Err(format!(
                "ffmpeg stopped reading its input and was stopped after the job ended: {}",
                log.trim()
            ));
        }
        let status = status?;
        if !status.success() {
            return Err(format!("ffmpeg failed ({status}): {}", log.trim()));
        }
        fed
    }
}

/// Stops `child` and reaps it.
fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The feeder thread: writes each frame to ffmpeg's stdin in order, then flushes and closes it.
/// A write error is kept for the app thread (which sees the queue close) and ends the thread.
fn feed_ffmpeg(
    mut pipe: BufWriter<ChildStdin>,
    frames: &Receiver<Vec<u8>>,
    slots: &Slots,
    failed: &Mutex<Option<String>>,
) -> Result<(), String> {
    let fail = |error: String| {
        failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(|| error.clone());
        Err(error)
    };
    while let Ok(rgba) = frames.recv() {
        // The frame left the queue: its place is free again.
        slots.release();
        let _span = tracing::debug_span!(target: "regia_agent", "render_out_ffmpeg_write").entered();
        if let Err(e) = pipe.write_all(&rgba) {
            return fail(format!("ffmpeg stopped reading: {e}"));
        }
    }
    if let Err(e) = pipe.flush() {
        return fail(format!("ffmpeg stopped reading: {e}"));
    }
    Ok(())
}

/// The log thread: reads ffmpeg's error output to its end, keeping the first `MAX_FFMPEG_LOG`
/// bytes, so a full pipe never stops ffmpeg.
fn drain_log(mut stderr: ChildStderr) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stderr.read(&mut buf) {
            Ok(0) | Err(_) => return kept,
            Ok(n) => {
                let room = MAX_FFMPEG_LOG.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Sink {
        /// Hands over an 8-bit frame, waiting while the queue is full.
        fn write(&mut self, index: u64, rgba: Vec<u8>) -> Result<(), String> {
            self.write_frame(index, FrameData::Rgba8(rgba), Vec::new())
        }

        /// Hands over a frame with its AOVs, waiting while the queue is full.
        fn write_frame(&mut self, index: u64, data: FrameData, aovs: Vec<AovFile>) -> Result<(), String> {
            match self.hand_over(index, data, aovs, &|| false)? {
                None => Ok(()),
                Some(_) => Err("not handed over".into()),
            }
        }
    }
    use crate::render_out::plan::PngLevel;
    use crate::render_out::writer::write_png16;
    use serde_json::json;
    use std::fs::File;

    #[test]
    fn sixteen_bit_frames_decode_back_exactly_and_only_go_to_png() {
        let dir = regia_test_utils::unique_temp_dir("render-out-png16");
        let r = Request::parse(&json!({
            "out": dir.display().to_string(), "frames": 1, "width": 2, "height": 1, "bit_depth": 16
        }))
        .expect("request");
        let mut sink = Sink::open(&r).expect("sink");
        let frame = [0u16, 1, 0x1234, 65535, 40_000, 257, 65534, 0x8000];
        sink.write_frame(0, FrameData::Rgba16(frame.to_vec()), Vec::new())
            .expect("frame 0");
        let files = sink.finish().files;
        let decoder = png::Decoder::new(std::io::BufReader::new(File::open(&files[0]).expect("open")));
        let mut reader = decoder.read_info().expect("header");
        assert_eq!(reader.info().bit_depth, png::BitDepth::Sixteen);
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        let info = reader.next_frame(&mut buf).expect("frame");
        let (pairs, _) = buf[..info.buffer_size()].as_chunks::<2>();
        let decoded: Vec<u16> = pairs.iter().map(|b| u16::from_be_bytes(*b)).collect();
        assert_eq!(decoded, frame);
        // The same frame gives the same file bytes (deterministic encoder).
        let again = dir.join("again.png");
        write_png16(&again, &frame, 2, 1, PngLevel::Fast).expect("again");
        assert_eq!(
            std::fs::read(&files[0]).expect("read"),
            std::fs::read(&again).expect("read")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn request(format: &str) -> Request {
        Request::parse(&json!({
            "out": "/o", "frames": 3, "format": format, "width": 640, "height": 360,
            "fps": 30000, "fps_den": 1001, "name": "cut1"
        }))
        .expect("request")
    }

    fn has_pair(args: &[String], key: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == key && w[1] == value)
    }

    #[test]
    fn every_movie_reads_raw_rgba_at_the_delivered_size_and_rate() {
        for format in ["ffv1", "prores", "dnxhr"] {
            let args = ffmpeg_args(&request(format));
            assert!(has_pair(&args, "-f", "rawvideo"), "{format}");
            assert!(has_pair(&args, "-pix_fmt", "rgba"), "{format}");
            assert!(has_pair(&args, "-s", "640x360"), "{format}");
            assert!(has_pair(&args, "-framerate", "30000/1001"), "{format}");
            assert!(has_pair(&args, "-i", "-"), "{format}");
            assert!(args.contains(&"-nostdin".to_string()), "{format}");
        }
    }

    #[test]
    fn each_format_picks_its_codec_and_container() {
        // The movie path is handed to ffmpeg, so it is the host's native path.
        let movie = |file: &str| std::path::Path::new("/o").join(file).display().to_string();
        let ffv1 = ffmpeg_args(&request("ffv1"));
        assert!(has_pair(&ffv1, "-c:v", "ffv1") && has_pair(&ffv1, "-g", "1"));
        assert_eq!(ffv1.last(), Some(&movie("cut1.mkv")));
        let prores = ffmpeg_args(&request("prores"));
        assert!(has_pair(&prores, "-c:v", "prores_ks") && has_pair(&prores, "-profile:v", "4444"));
        assert_eq!(prores.last(), Some(&movie("cut1.mov")));
        let dnxhr = ffmpeg_args(&request("dnxhr"));
        assert!(has_pair(&dnxhr, "-c:v", "dnxhd") && has_pair(&dnxhr, "-profile:v", "dnxhr_444"));
        assert!(ffmpeg_args(&request("png")).iter().all(|a| a != "-c:v"));
    }

    #[test]
    fn the_movie_is_the_delivered_size_even_when_supersampled() {
        let r = Request::parse(&json!({ "out": "/o", "frames": 1, "format": "ffv1", "width": 100, "height": 50, "supersample": 3 }))
            .expect("request");
        assert!(has_pair(&ffmpeg_args(&r), "-s", "100x50"));
    }

    #[test]
    fn ffmpeg_is_found_only_when_it_is_on_the_path() {
        let dir = std::env::temp_dir().join(format!("regia-render-out-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        let path = std::env::join_paths([&dir]).expect("path");
        assert_eq!(find_on_path("ffmpeg", Some(path.as_os_str())), None);
        std::fs::write(dir.join(name), b"").expect("file");
        assert_eq!(
            find_on_path("ffmpeg", Some(path.as_os_str())),
            Some(dir.join(name))
        );
        assert_eq!(find_on_path("ffmpeg", None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stand-in for `ffmpeg` that never reads its input (a stalled encoder or a full disk).
    #[cfg(unix)]
    fn deaf_ffmpeg(dir: &std::path::Path) -> std::path::PathBuf {
        let script = dir.join("deaf-ffmpeg");
        // Written by a child process: a sibling test forking while this process held a write
        // handle made the spawn fail with ETXTBSY (integ-v048d, ADR-5959).
        regia_test_utils::process::write_executable_script(&script, "#!/bin/sh\nexec sleep 120\n")
            .expect("script");
        script
    }

    #[cfg(unix)]
    fn big_movie_request(dir: &std::path::Path) -> Request {
        Request::parse(&json!({
            "out": dir.display().to_string(), "frames": 8, "format": "ffv1", "width": 1024, "height": 1024
        }))
        .expect("request")
    }

    /// Audit 11-03 agent H2: a movie frame was written to ffmpeg's stdin on the app thread, so an
    /// encoder that stopped reading parked the app inside frame delivery for good (no agent
    /// request, `render.out_cancel` included, was handled again). The hang detector below is a
    /// bound on a wait that never ends before the fix, not a timing budget.
    #[cfg(unix)]
    #[test]
    fn a_movie_frame_is_handed_over_while_ffmpeg_reads_nothing() {
        let dir = regia_test_utils::unique_temp_dir("render-out-deaf-ffmpeg");
        let r = big_movie_request(&dir);
        let mut sink = Sink::open_movie(&r, &deaf_ffmpeg(&dir)).expect("sink");
        let (done, returned) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // 4 MiB: far more than a pipe and the 1 MiB write buffer hold.
            let handed = sink.write(0, vec![0; 1024 * 1024 * 4]);
            let _ = done.send(handed.is_ok());
        });
        assert_eq!(
            returned.recv_timeout(std::time::Duration::from_secs(60)),
            Ok(true),
            "handing a frame over must not wait for ffmpeg to read it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hands frames to a movie whose ffmpeg reads nothing until its feed is full: the feeder sits
    /// in frame 0's write and `FEED_QUEUE` more wait, so the next hand-over can only wait.
    #[cfg(unix)]
    fn fill_stalled_movie(sink: &mut Sink) -> u64 {
        let frames = FEED_QUEUE as u64 + 1;
        for index in 0..frames {
            let back = sink
                .hand_over(
                    index,
                    FrameData::Rgba8(vec![0; 1024 * 1024 * 4]),
                    Vec::new(),
                    &|| false,
                )
                .expect("handed over");
            assert!(back.is_none(), "frame {index} fits the feed");
        }
        frames
    }

    /// ADR-2396: a cancel request queued while the app thread waits on a full, stalled movie
    /// feed ends the wait with the frame given back, and closing the stalled movie stops ffmpeg
    /// after the grace instead of waiting for it for good.
    #[cfg(unix)]
    #[test]
    fn a_cancel_request_ends_a_wait_on_a_stalled_movie_and_closing_stops_ffmpeg() {
        let dir = regia_test_utils::unique_temp_dir("render-out-stalled-ffmpeg");
        let r = big_movie_request(&dir);
        let mut sink = Sink::open_movie(&r, &deaf_ffmpeg(&dir)).expect("sink");
        let next = fill_stalled_movie(&mut sink);
        let seen = super::super::cancel_notes();
        let (done, returned) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let interrupted = move || super::super::cancel_notes() != seen;
            let back = sink.hand_over(next, FrameData::Rgba8(vec![7; 16]), Vec::new(), &interrupted);
            let _ = done.send((back.map(|b| b.is_some()), sink));
        });
        super::super::note_cancel_request();
        let (back, sink) = returned
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the cancel request ends the wait");
        assert_eq!(back, Ok(true), "the frame comes back, nothing is handed over");
        let (closed, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = closed.send(sink.finish());
        });
        let finished = finished
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("closing a stalled movie ends");
        assert!(finished.files.is_empty());
        let error = finished.error.expect("a stopped ffmpeg fails the movie");
        assert!(error.contains("stopped"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn png_frames_land_next_to_each_other_and_decode_back() {
        let dir = std::env::temp_dir().join(format!("regia-render-out-png-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let r = Request::parse(
            &json!({ "out": dir.display().to_string(), "frames": 2, "width": 2, "height": 1 }),
        )
        .expect("request");
        let mut sink = Sink::open(&r).expect("sink");
        let frame = [10u8, 20, 30, 255, 40, 50, 60, 255];
        sink.write(0, frame.to_vec()).expect("frame 0");
        sink.write(1, frame.to_vec()).expect("frame 1");
        let finished = sink.finish();
        assert_eq!((finished.frames, finished.error), (Some(2), None));
        let files = finished.files;
        assert_eq!(files, [r.frame_path(0), r.frame_path(1)]);
        let decoder = png::Decoder::new(std::io::BufReader::new(File::open(&files[1]).expect("open")));
        let mut reader = decoder.read_info().expect("header");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        let info = reader.next_frame(&mut buf).expect("frame");
        assert_eq!(&buf[..info.buffer_size()], frame);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
