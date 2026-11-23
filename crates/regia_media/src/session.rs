// SPDX-License-Identifier: MPL-2.0
//! A decode session: demuxer plus guarded decoders, yielding pictures in presentation order and
//! pushing decoded audio into an [`AudioSink`]. [`Session`] runs on the calling thread (blocking: the
//! `Lockstep` path, tests, the benchmark); [`WorkerFeed`] runs the same session on its own thread with a
//! bounded frame queue so the main thread never blocks outside `Lockstep` (specs/50 § 10).

use std::collections::VecDeque;
use std::io::{Read, Seek};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, unbounded};
use serde::{Deserialize, Serialize};

use crate::codec::{self, DecodedPicture};
use crate::container::{Demuxer, MediaInfo, MkvDemuxer, Packet, TrackKind};
use crate::error::MediaError;
use crate::isolate::{DecodeCaps, GuardStats, Guarded, GuardedAudio, GuardedVideo};

/// Where decoded audio goes (the `regia_audio` PCM feed in a running game, a buffer in tests).
pub trait AudioSink: Send {
    /// Appends interleaved samples that start at media time `pts_ns`.
    fn push(&mut self, pts_ns: i64, samples: &[f32]);
    /// Drops buffered audio (seek, close).
    fn flush(&mut self);
}

/// An [`AudioSink`] that collects samples (tests, offline checks).
#[derive(Debug, Default, Clone)]
pub struct CollectSink {
    /// Collected samples.
    pub samples: Vec<f32>,
    /// Flush count.
    pub flushes: u32,
}

impl AudioSink for CollectSink {
    fn push(&mut self, _pts_ns: i64, samples: &[f32]) {
        self.samples.extend_from_slice(samples);
    }

    fn flush(&mut self) {
        self.samples.clear();
        self.flushes += 1;
    }
}

/// Which audio track a session plays.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AudioChoice {
    /// The default rule (default flag, then language, then first).
    #[default]
    Default,
    /// A track by index in `MediaInfo::tracks`.
    Index(u32),
    /// No audio.
    None,
}

/// Session options.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionOptions {
    /// Decoder caps.
    pub caps: DecodeCaps,
    /// Audio track choice.
    pub audio: AudioChoice,
    /// System language for the default rule.
    pub language: String,
}

/// Session counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct FeedStats {
    /// Packets read from the container.
    pub packets_read: u64,
    /// Video guard counters.
    pub video: GuardStats,
    /// Audio guard counters.
    pub audio: GuardStats,
}

/// One poll of a frame feed.
#[derive(Debug)]
pub enum FeedPoll {
    /// The next picture.
    Frame(DecodedPicture),
    /// Nothing ready yet (non-blocking feeds only).
    Pending,
    /// End of stream.
    End,
    /// The session failed.
    Failed(MediaError),
}

/// A source of pictures in presentation order.
pub trait FrameFeed: Send {
    /// The next picture; with `block`, waits until one exists or the stream ends.
    fn poll(&mut self, block: bool) -> FeedPoll;
    /// Restarts at the cue point at or before `to_ns`; pictures before it are the caller's to discard.
    fn seek(&mut self, to_ns: u64);
    /// [`Self::seek`] for `SeekMode::Accurate`: decoded audio before `to_ns` is dropped too, so the
    /// audio between the cue point and the target (whose pictures the caller discards) is never
    /// heard (specs/50 § 2.5). The default is a plain seek.
    fn seek_accurate(&mut self, to_ns: u64) {
        self.seek(to_ns);
    }
    /// Counters.
    fn stats(&self) -> FeedStats;
    /// Ring counters when the feed decodes into a bounded ring (`crate::ring::RingFeed`).
    fn ring(&self) -> Option<crate::ring::RingStats> {
        None
    }
}

/// A decode session on the calling thread.
pub struct Session {
    demux: Box<dyn Demuxer>,
    info: MediaInfo,
    video_track: u64,
    video: GuardedVideo,
    audio: Option<(u64, u32, GuardedAudio)>,
    sink: Option<Box<dyn AudioSink>>,
    ready: VecDeque<DecodedPicture>,
    scratch: Vec<f32>,
    ended: bool,
    failed: Option<MediaError>,
    packets_read: u64,
    /// After an accurate seek: decoded audio before this media time is dropped.
    audio_from_ns: Option<i64>,
    /// Set by a threaded feed's drop: the packet loop stops between packets.
    stop: Option<Arc<AtomicBool>>,
    /// Text tracks by Matroska number, with their track index (ADR-2410).
    text_tracks: Vec<(u64, u32)>,
    cues: Option<crate::cues::CueStore>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("video", &self.video)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Opens a file: probes it, applies the codec rule and caps, and builds the decoders.
    pub fn open<R: Read + Seek + Send + 'static>(
        reader: R,
        opts: &SessionOptions,
        sink: Option<Box<dyn AudioSink>>,
    ) -> Result<Self, MediaError> {
        let demux = MkvDemuxer::open(reader)?;
        Self::from_demuxer(Box::new(demux), opts, sink)
    }

    /// Builds a session over any demuxer.
    pub fn from_demuxer(
        mut demux: Box<dyn Demuxer>,
        opts: &SessionOptions,
        sink: Option<Box<dyn AudioSink>>,
    ) -> Result<Self, MediaError> {
        let info = demux.probe()?;
        if let Some(e) = info.reject_reason() {
            return Err(e);
        }
        let video_info = info
            .video_track()
            .ok_or_else(|| MediaError::invalid("no video track"))?;
        for extra in info.tracks.iter().filter(|t| t.kind == TrackKind::Video).skip(1) {
            tracing::warn!(track = extra.index, "extra video track ignored");
        }
        if let Some(v) = &video_info.video {
            opts.caps.check_size(v.width, v.height)?;
        }
        let video = GuardedVideo::new(codec::video_decoder_with(video_info, &opts.caps)?, opts.caps);
        let audio_info = match &opts.audio {
            AudioChoice::None => None,
            AudioChoice::Default => info.default_audio_track(&opts.language),
            AudioChoice::Index(i) => Some(
                info.tracks
                    .iter()
                    .find(|t| t.index == *i && t.kind == TrackKind::Audio)
                    .ok_or(MediaError::TrackNotFound(*i))?,
            ),
        };
        let audio = match audio_info {
            Some(t) => match codec::audio_decoder_for(t) {
                Ok(d) => {
                    let channels = match d.output_format() {
                        Some((c, _)) => u32::from(c),
                        None => u32::from(t.audio.clone().unwrap_or_default().channels),
                    };
                    Some((t.number, channels, GuardedAudio::new(d, opts.caps)))
                }
                Err(e) => {
                    tracing::warn!(track = t.index, "audio track ignored: {e}");
                    None
                }
            },
            None => None,
        };
        let text_tracks = info
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Text && t.codec_id.starts_with("S_TEXT/"))
            .map(|t| (t.number, t.index))
            .collect();
        Ok(Self {
            text_tracks,
            cues: None,
            video_track: video_info.number,
            info,
            demux,
            video,
            audio,
            sink,
            ready: VecDeque::new(),
            scratch: Vec::new(),
            ended: false,
            failed: None,
            packets_read: 0,
            audio_from_ns: None,
            stop: None,
        })
    }

    /// File information.
    pub fn info(&self) -> &MediaInfo {
        &self.info
    }

    /// Whether an audio track is decoding.
    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// Channels and sample rate of the decoding audio track. A track without an Audio element
    /// has the Matroska defaults (one channel, 8000 Hz; deep-zai-media finding 4), so a decoding
    /// track always has a format and its audio reaches a feed.
    pub fn audio_format(&self) -> Option<(u8, u32)> {
        let (number, _, dec) = self.audio.as_ref()?;
        if let Some(format) = dec.output_format() {
            return Some(format);
        }
        let track = self.info.tracks.iter().find(|t| t.number == *number)?;
        let a = track.audio.clone().unwrap_or_default();
        Some((a.channels, a.sample_rate.round() as u32))
    }

    /// Sends the packets of the clip's text tracks into `store` (specs/50 § 2.8). Without a store
    /// text packets are skipped.
    pub fn set_cue_store(&mut self, store: Option<crate::cues::CueStore>) {
        self.cues = store;
    }

    /// Whether the clip has a text track this session can read (`S_TEXT/*`).
    pub fn has_text(&self) -> bool {
        !self.text_tracks.is_empty()
    }

    /// Replaces the audio sink.
    pub fn set_audio_sink(&mut self, sink: Option<Box<dyn AudioSink>>) {
        self.sink = sink;
    }

    /// The video decoder's name.
    pub fn decoder_name(&self) -> &'static str {
        self.video.name()
    }

    fn handle_packet(&mut self, p: Packet) -> Result<(), MediaError> {
        let rate = self.audio_format().map_or(48_000, |(_, r)| r);
        if p.track == self.video_track {
            if let Guarded::Ok(mut pictures) = self.video.decode(&p)? {
                pictures.sort_by_key(|pic| pic.pts_ns);
                self.ready.extend(pictures);
            }
        } else if let Some(&(_, index)) = self.text_tracks.iter().find(|(n, _)| *n == p.track) {
            if let Some(store) = &self.cues {
                store.push(index, p.pts_ns, p.duration_ns, &p.data);
            }
        } else if let Some((track, channels, dec)) = &mut self.audio
            && p.track == *track
        {
            self.scratch.clear();
            dec.decode(
                &p,
                &mut self.scratch,
                silence_samples(*channels, rate, p.duration_ns),
            )?;
            let channels = (*channels).max(1) as usize;
            let mut keep = &self.scratch[..];
            let mut pts = p.pts_ns;
            if let Some(from) = self.audio_from_ns {
                // An accurate seek: drop the audio before its target (the caller discards the
                // pictures there), cutting the packet that holds the target.
                let late_ns = from.saturating_sub(p.pts_ns).max(0) as u128;
                let skip_frames = (late_ns * u128::from(rate) / 1_000_000_000) as usize;
                let skip = skip_frames.saturating_mul(channels).min(keep.len());
                if skip < keep.len() {
                    self.audio_from_ns = None;
                    pts = pts.max(from);
                }
                keep = &keep[skip..];
            }
            if let Some(sink) = &mut self.sink
                && !keep.is_empty()
            {
                sink.push(pts, keep);
            }
        }
        Ok(())
    }

    /// Whether a threaded feed's drop asked the decode loop to stop.
    pub(crate) fn stop_requested(&self) -> bool {
        self.stop.as_ref().is_some_and(|s| s.load(Ordering::Acquire))
    }

    /// A stop flag for the thread this session moves to: once set, [`Self::next_picture`] returns
    /// [`FeedPoll::Pending`] before reading another packet (a feed being dropped).
    pub(crate) fn stop_flag(&mut self) -> Arc<AtomicBool> {
        Arc::clone(self.stop.get_or_insert_with(|| Arc::new(AtomicBool::new(false))))
    }

    /// The next picture, decoding as many packets as needed (blocking). [`FeedPoll::Pending`] only
    /// when the threaded feed that owns this session is being dropped.
    pub fn next_picture(&mut self) -> FeedPoll {
        if let Some(e) = &self.failed {
            return FeedPoll::Failed(e.clone());
        }
        loop {
            if let Some(p) = self.ready.pop_front() {
                return FeedPoll::Frame(p);
            }
            if self.ended {
                return FeedPoll::End;
            }
            // Checked between packets: an audio-only tail (or a long run of packets that yield no
            // picture) would otherwise keep a dropped feed's thread decoding to the next picture.
            if self.stop_requested() {
                return FeedPoll::Pending;
            }
            let packet = match self.demux.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => {
                    self.ended = true;
                    let mut held = self.video.drain();
                    held.sort_by_key(|pic| pic.pts_ns);
                    self.ready.extend(held);
                    continue;
                }
                Err(e) => {
                    self.failed = Some(e.clone());
                    return FeedPoll::Failed(e);
                }
            };
            self.packets_read += 1;
            if let Err(e) = self.handle_packet(packet) {
                self.failed = Some(e.clone());
                return FeedPoll::Failed(e);
            }
        }
    }
}

/// Samples of silence standing in for a rejected audio packet: its duration at the output rate, or 20 ms
/// when the block carried none.
fn silence_samples(channels: u32, rate: u32, duration_ns: Option<u64>) -> usize {
    let ns = duration_ns.filter(|&d| d > 0).unwrap_or(20_000_000);
    let frames = (u128::from(ns) * u128::from(rate)).div_ceil(1_000_000_000);
    // 120 ms is the longest Opus packet; nothing legitimate is longer than a few seconds.
    let frames = (frames as usize).min(rate as usize * 10);
    frames * channels as usize
}

impl FrameFeed for Session {
    fn poll(&mut self, _block: bool) -> FeedPoll {
        self.next_picture()
    }

    fn seek(&mut self, to_ns: u64) {
        self.audio_from_ns = None;
        self.ready.clear();
        self.ended = false;
        self.video.flush();
        if let Some((_, _, a)) = &mut self.audio {
            a.flush();
        }
        if let Some(sink) = &mut self.sink {
            sink.flush();
        }
        if let Err(e) = self.demux.seek(to_ns) {
            self.failed = Some(e);
        }
    }

    fn seek_accurate(&mut self, to_ns: u64) {
        FrameFeed::seek(self, to_ns);
        self.audio_from_ns = Some(to_ns.min(i64::MAX as u64) as i64);
    }

    fn stats(&self) -> FeedStats {
        FeedStats {
            packets_read: self.packets_read,
            video: self.video.stats(),
            audio: self.audio.as_ref().map(|(_, _, a)| a.stats()).unwrap_or_default(),
        }
    }
}

enum Cmd {
    /// Generation, target, accurate.
    Seek(u64, u64, bool),
    Stop,
}

/// A session on its own thread, with a bounded queue of decoded pictures. Commands carry a generation
/// so pictures decoded before a seek are discarded.
pub struct WorkerFeed {
    cmd: Sender<Cmd>,
    out: Receiver<(u64, FeedPoll, FeedStats)>,
    generation: u64,
    stats: FeedStats,
    finished: bool,
    /// The terminal poll (`End`, or `Failed` with its error) of the current generation: the worker
    /// goes idle after it and sends nothing more, so later polls answer it again until a seek.
    terminal: Option<Option<MediaError>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Disconnects when the decode thread ends (its sender drops, on return or unwind).
    done: Receiver<()>,
}

/// How long dropping a feed waits for its decode thread. A thread blocked in a read of a
/// stalled pipe or network stream gets no further: it is detached (it ends when its read
/// returns) so the dropping thread never hangs (audit-v035-conc M6).
pub(crate) const DECODE_THREAD_JOIN_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

impl std::fmt::Debug for WorkerFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerFeed")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl WorkerFeed {
    /// Moves `session` to a new thread; `queue_frames` bounds the decoded queue (4 by default, max 8).
    pub fn spawn(mut session: Session, queue_frames: usize) -> Result<Self, MediaError> {
        let (cmd_tx, cmd_rx) = unbounded::<Cmd>();
        let (out_tx, out_rx) = bounded(queue_frames.clamp(1, 8));
        let stop = session.stop_flag();
        let (done_tx, done_rx) = bounded::<()>(0);
        let thread = std::thread::Builder::new()
            .name("regia-video-decode".into())
            .spawn(move || {
                let _done = done_tx;
                let mut generation = 0u64;
                let mut idle = false;
                loop {
                    let cmd = if idle {
                        cmd_rx.recv().ok()
                    } else {
                        match cmd_rx.try_recv() {
                            Ok(c) => Some(c),
                            Err(TryRecvError::Empty) => None,
                            Err(TryRecvError::Disconnected) => return,
                        }
                    };
                    match cmd {
                        Some(Cmd::Stop) => return,
                        Some(Cmd::Seek(g, to, accurate)) => {
                            generation = g;
                            if accurate {
                                session.seek_accurate(to);
                            } else {
                                session.seek(to);
                            }
                            idle = false;
                            continue;
                        }
                        None if idle => return,
                        None => {}
                    }
                    let _span = tracing::trace_span!("video_decode").entered();
                    let poll = session.next_picture();
                    if session.stop_requested() {
                        return;
                    }
                    idle = !matches!(poll, FeedPoll::Frame(_));
                    let stats = FrameFeed::stats(&session);
                    if out_tx.send((generation, poll, stats)).is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| MediaError::DecodeFailed(format!("cannot start the decode thread: {e}")))?;
        Ok(Self {
            cmd: cmd_tx,
            out: out_rx,
            generation: 0,
            stats: FeedStats::default(),
            finished: false,
            terminal: None,
            stop,
            thread: Some(thread),
            done: done_rx,
        })
    }
}

impl WorkerFeed {
    fn send_seek(&mut self, to_ns: u64, accurate: bool) {
        self.generation += 1;
        self.terminal = None;
        // Unbounded: a seek never blocks the main thread; stale generations are skipped on poll.
        let _ = self.cmd.send(Cmd::Seek(self.generation, to_ns, accurate));
    }
}

impl FrameFeed for WorkerFeed {
    fn poll(&mut self, block: bool) -> FeedPoll {
        if self.finished {
            return FeedPoll::End;
        }
        match &self.terminal {
            Some(None) => return FeedPoll::End,
            Some(Some(e)) => return FeedPoll::Failed(e.clone()),
            None => {}
        }
        loop {
            let item = if block {
                match self.out.recv() {
                    Ok(i) => i,
                    Err(_) => {
                        return FeedPoll::Failed(MediaError::DecodeFailed("decode thread ended".into()));
                    }
                }
            } else {
                match self.out.try_recv() {
                    Ok(i) => i,
                    Err(TryRecvError::Empty) => return FeedPoll::Pending,
                    Err(TryRecvError::Disconnected) => {
                        return FeedPoll::Failed(MediaError::DecodeFailed("decode thread ended".into()));
                    }
                }
            };
            let (generation, poll, stats) = item;
            if generation != self.generation {
                continue;
            }
            self.stats = stats;
            match &poll {
                FeedPoll::End => self.terminal = Some(None),
                FeedPoll::Failed(e) => self.terminal = Some(Some(e.clone())),
                FeedPoll::Frame(_) | FeedPoll::Pending => {}
            }
            return poll;
        }
    }

    fn seek(&mut self, to_ns: u64) {
        self.send_seek(to_ns, false);
    }

    fn seek_accurate(&mut self, to_ns: u64) {
        self.send_seek(to_ns, true);
    }

    fn stats(&self) -> FeedStats {
        self.stats
    }
}

impl Drop for WorkerFeed {
    fn drop(&mut self) {
        // Seen between packets, so a worker deep in packets that yield no picture stops too.
        self.stop.store(true, Ordering::Release);
        let _ = self.cmd.send(Cmd::Stop);
        // Unblock a worker waiting on a full output queue.
        while self.out.try_recv().is_ok() {}
        self.finished = true;
        if let Some(t) = self.thread.take() {
            drop(std::mem::replace(&mut self.out, bounded(1).1));
            // Joined once it ended; a thread still inside a blocking read is detached.
            match self.done.recv_timeout(DECODE_THREAD_JOIN_WAIT) {
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    tracing::warn!(target: "regia_media", "video decode thread still busy at drop; detached");
                }
                _ => {
                    let _ = t.join();
                }
            }
        }
    }
}

#[cfg(test)]
mod silence_tests {
    use super::silence_samples;

    #[test]
    fn rejected_audio_silence_follows_duration_and_rate() {
        // No duration: 20 ms.
        assert_eq!(silence_samples(2, 48_000, None), 2 * 960);
        assert_eq!(silence_samples(1, 44_100, None), 882);
        // A 120 ms Opus packet at 48 kHz stereo.
        assert_eq!(silence_samples(2, 48_000, Some(120_000_000)), 2 * 5760);
        assert_eq!(silence_samples(2, 44_100, Some(100_000_000)), 2 * 4410);
        // A hostile duration is capped.
        assert_eq!(silence_samples(1, 48_000, Some(u64::MAX)), 480_000);
    }
}
