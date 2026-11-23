// SPDX-License-Identifier: MPL-2.0
//! The network recorder (spec 103, ADR-3021): a virtual, non-owner connection on the server that receives what a
//! client would, and stores it in a `.netdemo` file ([`format`](mod@format), [`sink`]) or a memory ring ([`ring`]). Playback runs
//! through the normal client path ([`playback`], `regia_framework::net_replay`), with no server.
//!
//! | Item | What |
//! |---|---|
//! | [`start_recording`] / [`stop_recording`] | A file, written by a thread, renamed into place at close |
//! | [`start_ring`] / [`stop_ring`] / [`export_ring`] | The last N seconds in a fixed block of memory, and a file from it |
//! | [`recorder_status`] | What `net.replay_state` reports |
//! | [`format::NetDemoReader`] | A checked file: typed errors, no trust in any length |
//!
//! A recorder changes nothing for the session: real clients' bytes are the same with one and without one
//! (spec 103 § 3.4).

pub mod format;
pub mod playback;
pub mod ring;
pub mod sink;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;

use crate::bits::BitWriter;
use crate::message::Channel;
use crate::model::{NetMode, NetTick};
use crate::replication::{GatheredConn, ServerReplication};
use crate::runtime::{self, NetNode, TAG_TICK};
use crate::session::{NetSession, PeerId};

pub use format::{
    EndReason, NETDEMO_FORMAT_VERSION, NetDemoError, NetDemoHeader, NetDemoReader, RecorderView,
};
pub use playback::NetReplayClock;
pub use ring::RingStatus;

/// Peer ids from here up are virtual connections: no socket, never listed by [`NetNode::peers`].
pub const FIRST_VIRTUAL_PEER: PeerId = PeerId(0xFFFF_FF00);
/// The connection that receives the recorded stream.
pub const RECORDER_PEER: PeerId = PeerId(0xFFFF_FF00);
/// The connection that exists for the gather of one keyframe tick.
pub const KEYFRAME_PEER: PeerId = PeerId(0xFFFF_FF01);
/// The longest segment in ticks, so a segment always fits one client apply (`runtime::MAX_PENDING_FRAMES`, with room
/// for each tick's `UMSG` frame).
pub const MAX_SEGMENT_TICKS: u32 = 2048;
/// The largest ring (spec 103 § 7): it is allocated whole at the start.
pub const MAX_RING_BYTES: usize = 4 << 30;

/// Whether a peer id names a virtual connection.
#[must_use]
pub fn is_virtual_peer(peer: PeerId) -> bool {
    peer.0 >= FIRST_VIRTUAL_PEER.0
}

/// How to record to a file (spec 103 § 4).
#[derive(Debug, Clone, PartialEq)]
pub struct RecorderConfig {
    /// Which entities it holds.
    pub view: RecorderView,
    /// Seconds between keyframes: a seek replays at most this much. Default 5.
    pub keyframe_every_s: f32,
    /// The largest file; the recording ends cleanly within it. 0 = no limit. Default 2 GiB.
    pub max_bytes: u64,
    /// The longest recording in seconds. 0 = no limit. Default 4 hours.
    pub max_seconds: f32,
    /// The level's name for the header, a label only; empty takes the session's. A session that travels ends the file
    /// whatever this says.
    pub level: String,
    /// Replace a file already at the path. Default off.
    pub overwrite: bool,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self {
            view: RecorderView::Everything,
            keyframe_every_s: 5.0,
            max_bytes: 2 << 30,
            max_seconds: 4.0 * 3600.0,
            level: String::new(),
            overwrite: false,
        }
    }
}

/// How to keep a ring (spec 103 § 7).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RingConfig {
    /// Which entities it holds.
    pub view: RecorderView,
    /// History to keep, in seconds. Default 30.
    pub seconds: f32,
    /// The memory bound, allocated at the start. Default 16 MiB.
    pub max_bytes: usize,
    /// Seconds between keyframes. Default 5.
    pub keyframe_every_s: f32,
}

impl Default for RingConfig {
    fn default() -> Self {
        Self {
            view: RecorderView::Everything,
            seconds: 30.0,
            max_bytes: 16 << 20,
            keyframe_every_s: 5.0,
        }
    }
}

/// A finished file.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingSummary {
    /// Where it is.
    pub path: PathBuf,
    /// Its size.
    pub bytes: u64,
    /// Tick chunks.
    pub ticks: u64,
    /// Keyframes.
    pub keyframes: u64,
    /// The first tick.
    pub first_tick: NetTick,
    /// The last tick.
    pub last_tick: NetTick,
    /// Why it ended.
    pub reason: EndReason,
}

/// The file recording's state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FileStatus {
    /// Where it will be.
    pub path: PathBuf,
    /// Bytes so far.
    pub bytes: u64,
    /// Tick chunks so far.
    pub ticks: u64,
    /// Keyframes so far.
    pub keyframes: u64,
    /// The first tick, once recording began.
    pub first_tick: Option<NetTick>,
    /// The last tick so far.
    pub last_tick: Option<NetTick>,
    /// Why it ended, once a cap or a level change ended it (`stop_recording` collects the file).
    pub ended: Option<EndReason>,
    /// What failed, once a write error ended it (`netdemo_io: ...`; `stop_recording` returns it).
    pub error: Option<String>,
}

/// What `net.replay_state` reports on a server.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RecorderStatus {
    /// The connection's view, while one runs.
    pub view: Option<RecorderView>,
    /// A `Player` view's peer left: its set is frozen.
    pub view_peer_gone: bool,
    /// The file recording.
    pub file: Option<FileStatus>,
    /// The ring.
    pub ring: Option<RingStatus>,
    /// Keyframes over the chunk limit, not stored.
    pub keyframes_skipped: u64,
    /// Ticks over the chunk limit, not stored: a keyframe follows at once, so the state comes back (spec 103 § 5).
    pub ticks_skipped: u64,
    /// How often a working buffer of the tick path had to grow: flat after warm-up (spec 103 § 10).
    pub scratch_grows: u64,
    /// Ticks recorded on this connection.
    pub ticks: u64,
}

#[derive(Debug)]
struct FileRecording {
    sink: sink::FileSink,
    path: PathBuf,
    max_bytes: u64,
    max_seconds: f32,
    keyframe_every_ticks: u32,
    header: Option<NetDemoHeader>,
    ticks: u64,
    keyframes: u64,
    first_tick: Option<NetTick>,
    last_tick: Option<NetTick>,
}

#[derive(Debug)]
struct RingRecording {
    ring: ring::Ring,
    keyframe_every_ticks: u32,
}

/// A file that ended without a `stop_recording`, waiting for one.
#[derive(Debug)]
enum Finished {
    /// A cap ended it; the writer thread is syncing and renaming it.
    Closing {
        closing: sink::ClosingSink,
        summary: RecordingSummary,
    },
    /// It failed.
    Failed { path: PathBuf, error: NetDemoError },
}

/// The recorder's state on a server: at most one connection, feeding a file, a ring, or both. A world dropped with a
/// file still recording closes it (spec 103 § 6): a clean exit keeps the recording.
#[derive(Resource, Debug, Default)]
pub struct NetRecorder {
    view: Option<RecorderView>,
    file: Option<FileRecording>,
    /// A file ended by a cap or a failure, waiting for `stop_recording`.
    finished: Option<Finished>,
    ring: Option<RingRecording>,
    /// The session's level: travel ends a file and starts the ring over. Apart from a file's header label.
    session_level: String,
    hz: f64,
    send_every: u32,
    last_keyframe: Option<NetTick>,
    force_keyframe: bool,
    keyframe_wanted: bool,
    /// The keyframe of this tick, encoded (empty = none).
    keyframe_chunk: Vec<u8>,
    /// The tick chunk being built.
    chunk: Vec<u8>,
    chunk_start: usize,
    frames_this_tick: u32,
    keyframes_skipped: u64,
    ticks_skipped: u64,
    scratch_grows: u64,
    ticks: u64,
    view_peer_gone: bool,
}

impl Drop for NetRecorder {
    fn drop(&mut self) {
        if let Some(f) = self.file.take()
            && let Err(e) = close_file(f, EndReason::Stopped)
        {
            tracing::warn!(target: "net", error = %e, "net.recording_lost_at_exit");
        }
    }
}

impl NetRecorder {
    fn every_ticks(&self) -> u32 {
        let file = self.file.as_ref().map(|f| f.keyframe_every_ticks);
        let ring = self.ring.as_ref().map(|r| r.keyframe_every_ticks);
        file.into_iter().chain(ring).min().unwrap_or(MAX_SEGMENT_TICKS)
    }

    fn keyframe_due(&self, tick: NetTick) -> bool {
        self.force_keyframe
            || self
                .last_keyframe
                .is_none_or(|last| tick.0.saturating_sub(last.0) >= self.every_ticks())
    }

    /// Starts this tick's chunk.
    pub fn begin_tick(&mut self, tick: NetTick) {
        self.chunk.clear();
        self.frames_this_tick = 0;
        self.chunk_start = format::begin_chunk(&mut self.chunk, format::ChunkKind::Tick, tick);
    }

    /// Adds one frame of the tick.
    pub fn add_frame(&mut self, channel: Channel, bytes: &[u8]) {
        let capacity = self.chunk.capacity();
        format::put_frame(&mut self.chunk, channel, bytes);
        if self.chunk.capacity() != capacity {
            self.scratch_grows += 1;
        }
        self.frames_this_tick += 1;
    }

    /// Stores this tick's keyframe, encoded.
    pub fn set_keyframe(&mut self, tick: NetTick, frame: &[u8]) {
        let capacity = self.keyframe_chunk.capacity();
        self.keyframe_chunk.clear();
        let start = format::begin_chunk(&mut self.keyframe_chunk, format::ChunkKind::Keyframe, tick);
        self.keyframe_chunk.extend_from_slice(frame);
        if !format::finish_chunk(&mut self.keyframe_chunk, start) {
            self.keyframe_chunk.clear();
            self.keyframes_skipped += 1;
            tracing::warn!(target: "net", tick = tick.0, bytes = frame.len(), "net.recorder_keyframe_too_large");
        }
        if self.keyframe_chunk.capacity() != capacity {
            self.scratch_grows += 1;
        }
    }

    /// Finishes the tick: the keyframe and the tick chunk go to the file and the ring. Caps end the file here.
    pub fn end_tick(&mut self, tick: NetTick) {
        if !format::finish_chunk(&mut self.chunk, self.chunk_start) {
            self.chunk.clear();
            // What the chunk held is gone: the next keyframe brings the state back.
            self.ticks_skipped += 1;
            self.force_keyframe = true;
            tracing::warn!(target: "net", tick = tick.0, "net.recorder_tick_too_large");
        }
        self.ticks += 1;
        self.write_file(tick);
        if let Some(r) = self.ring.as_mut() {
            // A file's shorter interval does not cut the ring's segments: it has no use for that keyframe.
            if !self.keyframe_chunk.is_empty() && r.ring.wants_keyframe(tick) {
                r.ring.push(&self.keyframe_chunk, true, tick);
            }
            if !self.chunk.is_empty() {
                r.ring.push(&self.chunk, false, tick);
            }
        }
        self.keyframe_chunk.clear();
    }

    /// One whole tick in one call: the path a send tick takes, for tools and the allocation test.
    pub fn record_frames(&mut self, tick: NetTick, keyframe: Option<&[u8]>, frames: &[(Channel, &[u8])]) {
        if let Some(k) = keyframe {
            self.set_keyframe(tick, k);
        }
        self.begin_tick(tick);
        for (c, f) in frames {
            self.add_frame(*c, f);
        }
        self.end_tick(tick);
    }

    fn write_file(&mut self, tick: NetTick) {
        let Some(f) = self.file.as_mut() else {
            return;
        };
        let key = self.keyframe_chunk.as_slice();
        // A file starts at a keyframe: until then nothing is of use to it.
        if f.first_tick.is_none() && key.is_empty() {
            return;
        }
        if let Some(limit) = f.first_tick.filter(|_| f.max_seconds > 0.0)
            && f64::from(tick.0.saturating_sub(limit.0)) / self.hz >= f64::from(f.max_seconds)
        {
            self.finish_file(EndReason::TimeCap);
            return;
        }
        let header = f.header.take().map(|mut h| {
            h.first_tick = tick;
            h.encode()
        });
        let incoming = header.as_ref().map_or(0, Vec::len) + key.len() + self.chunk.len();
        if f.max_bytes > 0 && f.sink.written() + (incoming + format::END_CHUNK_BYTES) as u64 > f.max_bytes {
            self.finish_file(EndReason::SizeCap);
            return;
        }
        // Whole chunks only: what the end chunk counts is what the file holds.
        let mut failed = None;
        for (i, part) in [header.as_deref().unwrap_or(&[]), key, self.chunk.as_slice()]
            .into_iter()
            .enumerate()
        {
            if part.is_empty() {
                continue;
            }
            match f.sink.write(part) {
                Ok(true) => {
                    if i == 1 {
                        f.keyframes += 1;
                    }
                }
                Ok(false) => {
                    failed = Some(Ok(()));
                    break;
                }
                Err(e) => {
                    failed = Some(Err(e));
                    break;
                }
            }
        }
        match failed {
            None => {
                f.first_tick = f.first_tick.or(Some(tick));
                f.ticks += u64::from(!self.chunk.is_empty());
                // The reader takes the last tick from the tick chunks, and a tick over the chunk limit stored none.
                if !self.chunk.is_empty() {
                    f.last_tick = Some(tick);
                }
            }
            Some(Ok(())) => self.finish_file(EndReason::DiskSlow),
            Some(Err(e)) => self.fail_file(e),
        }
    }

    fn fail_file(&mut self, error: NetDemoError) {
        if let Some(f) = self.file.take() {
            let path = f.path.clone();
            // The write that failed already stopped the writer: this does not wait on the disk.
            f.sink.abandon();
            self.finished = Some(Finished::Failed { path, error });
        }
    }

    /// Ends the file from the tick path: the writer thread syncs and renames it, and the tick never waits for the disk.
    fn finish_file(&mut self, reason: EndReason) {
        let Some(f) = self.file.take() else {
            return;
        };
        self.finished = Some(match begin_close(f, reason) {
            Ok((closing, summary)) => Finished::Closing { closing, summary },
            Err((path, error)) => Finished::Failed { path, error },
        });
    }

    /// A session on another level: the file ends and the ring starts over (one recording holds one level).
    fn follow_session_level(&mut self, level: &str) {
        if level.is_empty() || level == self.session_level {
            return;
        }
        // A session hosted after the recording began names its level for the first time: nothing changed.
        let travelled = !self.session_level.is_empty();
        self.session_level.clear();
        self.session_level.push_str(level);
        if travelled {
            self.finish_file(EndReason::LevelChanged);
            if let Some(r) = self.ring.as_mut() {
                r.ring.restart();
            }
            self.force_keyframe = true;
        }
    }

    fn release_ready(&self) -> bool {
        self.view.is_some() && self.file.is_none() && self.ring.is_none()
    }

    /// The state for `net.replay_state`.
    #[must_use]
    pub fn status(&self) -> RecorderStatus {
        let mut file = self.file.as_ref().map(|f| FileStatus {
            path: f.path.clone(),
            bytes: f.sink.written(),
            ticks: f.ticks,
            keyframes: f.keyframes,
            first_tick: f.first_tick,
            last_tick: f.last_tick,
            ended: None,
            error: None,
        });
        match &self.finished {
            Some(Finished::Closing { summary: done, .. }) => {
                file = Some(FileStatus {
                    path: done.path.clone(),
                    bytes: done.bytes,
                    ticks: done.ticks,
                    keyframes: done.keyframes,
                    first_tick: Some(done.first_tick),
                    last_tick: Some(done.last_tick),
                    ended: Some(done.reason),
                    error: None,
                });
            }
            Some(Finished::Failed { path, error }) => {
                file = Some(FileStatus {
                    path: path.clone(),
                    error: Some(error.to_string()),
                    ..FileStatus::default()
                });
            }
            None => {}
        }
        RecorderStatus {
            view: self.view,
            view_peer_gone: self.view_peer_gone,
            file,
            ring: self.ring.as_ref().map(|r| r.ring.status()),
            keyframes_skipped: self.keyframes_skipped,
            ticks_skipped: self.ticks_skipped,
            scratch_grows: self.scratch_grows,
            ticks: self.ticks,
        }
    }
}

/// The tail goes to the writer thread, which syncs and renames: nothing here waits for the disk unless nothing was
/// recorded (and then the writer is idle).
fn begin_close(
    f: FileRecording,
    reason: EndReason,
) -> Result<(sink::ClosingSink, RecordingSummary), (PathBuf, NetDemoError)> {
    let FileRecording {
        sink,
        path,
        ticks,
        keyframes,
        first_tick,
        last_tick,
        ..
    } = f;
    // Nothing recorded yet: a file with no keyframe is not a recording.
    let (Some(first), Some(last)) = (first_tick, last_tick) else {
        sink.abandon();
        return Err((
            path,
            NetDemoError::Io("nothing was recorded: no send tick ran since the start".to_string()),
        ));
    };
    let mut tail = Vec::with_capacity(format::END_CHUNK_BYTES);
    format::put_end_chunk(
        &mut tail,
        &format::EndSummary {
            tick_chunks: ticks,
            keyframes,
            last_tick: last,
            reason,
        },
    );
    let bytes = sink.written() + tail.len() as u64;
    tracing::info!(target: "net", path = %path.display(), bytes, ticks, reason = reason.as_str(), "net.recording_closing");
    let closing = sink.begin_finish(&tail);
    Ok((
        closing,
        RecordingSummary {
            path,
            bytes,
            ticks,
            keyframes,
            first_tick: first,
            last_tick: last,
            reason,
        },
    ))
}

fn close_file(f: FileRecording, reason: EndReason) -> Result<RecordingSummary, NetDemoError> {
    let (closing, summary) = begin_close(f, reason).map_err(|(_, e)| e)?;
    collect(closing, summary)
}

fn collect(closing: sink::ClosingSink, summary: RecordingSummary) -> Result<RecordingSummary, NetDemoError> {
    closing.wait().map(|bytes| RecordingSummary { bytes, ..summary })
}

fn require_server(world: &World) -> Result<(), NetDemoError> {
    if world
        .get_resource::<NetMode>()
        .copied()
        .unwrap_or_default()
        .is_server()
    {
        Ok(())
    } else {
        Err(NetDemoError::NotServer)
    }
}

fn keyframe_ticks(seconds: f32, hz: f64) -> Result<u32, NetDemoError> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(NetDemoError::ConfigInvalid("keyframe_every_s must be above 0"));
    }
    Ok(((f64::from(seconds) * hz).round() as u32).clamp(1, MAX_SEGMENT_TICKS))
}

fn session_level(world: &World) -> String {
    world
        .get_resource::<NetSession>()
        .and_then(|s| s.server())
        .map(|s| s.level().to_string())
        .unwrap_or_default()
}

/// Brings the connection up with `view`, or checks that it already runs with that view.
fn ensure_connection(world: &mut World, view: RecorderView) -> Result<(), NetDemoError> {
    runtime::ensure_resources(world);
    if let RecorderView::Player(p) = view
        && !world.resource::<NetNode>().has_peer(p)
    {
        return Err(NetDemoError::ViewInvalid(p.0));
    }
    let hz = runtime::tick_hz(world);
    let every = world
        .resource::<crate::settings::NetSettings>()
        .replication
        .send_every_ticks
        .max(1);
    let level = session_level(world);
    let mut rec = world.get_resource_or_insert_with(NetRecorder::default);
    match rec.view {
        Some(running) if running != view => return Err(NetDemoError::ViewConflict),
        Some(_) => {
            rec.force_keyframe = true;
            return Ok(());
        }
        None => {}
    }
    rec.view = Some(view);
    rec.session_level = level;
    rec.hz = hz;
    rec.send_every = every;
    rec.force_keyframe = true;
    rec.view_peer_gone = false;
    runtime::server_add_virtual_peer(world, RECORDER_PEER);
    world
        .resource_mut::<ServerReplication>()
        .add_recorder_conn(RECORDER_PEER, view);
    Ok(())
}

fn release_connection(world: &mut World) {
    if world
        .get_resource::<NetRecorder>()
        .is_some_and(NetRecorder::release_ready)
    {
        runtime::server_remove_peer(world, RECORDER_PEER);
        if let Some(mut rec) = world.get_resource_mut::<NetRecorder>() {
            rec.view = None;
            rec.last_keyframe = None;
            rec.view_peer_gone = false;
        }
    }
}

/// Starts recording this server's stream to `path` (spec 103 § 4). The file appears there only when it closes.
///
/// # Errors
/// `net_not_server`, `recorder_busy`, `recorder_view_conflict`, `recorder_view_invalid`, `recorder_config_invalid`,
/// `netdemo_exists`, `netdemo_io`.
pub fn start_recording(world: &mut World, path: &Path, config: RecorderConfig) -> Result<(), NetDemoError> {
    require_server(world)?;
    if let Some(rec) = world.get_resource::<NetRecorder>()
        && (rec.file.is_some() || rec.finished.is_some())
    {
        return Err(NetDemoError::Busy("file"));
    }
    if config.max_seconds < 0.0 || !config.max_seconds.is_finite() {
        return Err(NetDemoError::ConfigInvalid("max_seconds"));
    }
    let hz = runtime::tick_hz(world);
    let every = keyframe_ticks(config.keyframe_every_s, hz)?;
    if config.max_bytes > 0 && config.max_bytes < 4096 {
        return Err(NetDemoError::ConfigInvalid(
            "max_bytes must be 0 or at least 4096",
        ));
    }
    if world
        .get_resource::<NetRecorder>()
        .and_then(|r| r.view)
        .is_some_and(|v| v != config.view)
    {
        return Err(NetDemoError::ViewConflict);
    }
    let sink = sink::FileSink::create(path, config.overwrite, &[])?;
    ensure_connection(world, config.view)?;
    // The label is the header's alone: what ends the file is the session's level moving (`follow_session_level`).
    let level = if config.level.is_empty() {
        session_level(world)
    } else {
        config.level.clone()
    };
    let header = make_header(world, every, config.view, &level);
    let mut rec = world.resource_mut::<NetRecorder>();
    rec.file = Some(FileRecording {
        sink,
        path: path.to_path_buf(),
        max_bytes: config.max_bytes,
        max_seconds: config.max_seconds,
        keyframe_every_ticks: every,
        header: Some(header),
        ticks: 0,
        keyframes: 0,
        first_tick: None,
        last_tick: None,
    });
    Ok(())
}

fn make_header(world: &World, every: u32, view: RecorderView, level: &str) -> NetDemoHeader {
    let (components, messages) = NetDemoHeader::tables_of(world);
    NetDemoHeader {
        net_protocol: crate::settings::NET_PROTOCOL_VERSION,
        protocol_hash: runtime::protocol_hash(world),
        tick_hz: runtime::tick_hz(world),
        send_every_ticks: world
            .resource::<crate::settings::NetSettings>()
            .replication
            .send_every_ticks
            .max(1),
        keyframe_every_ticks: every,
        first_tick: NetTick(0),
        view,
        level: level.to_string(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        components,
        messages,
    }
}

/// Stops the file recording, or collects one a cap already ended. This waits for the sync and the rename (spec 103
/// § 6), so the file is in place when it returns.
///
/// # Errors
/// `recorder_idle`, or what ended it: `netdemo_io`, or `netdemo_exists` when a file appeared at the path meanwhile (both
/// name where a whole recording stays).
pub fn stop_recording(world: &mut World) -> Result<RecordingSummary, NetDemoError> {
    let Some(mut rec) = world.get_resource_mut::<NetRecorder>() else {
        return Err(NetDemoError::Idle);
    };
    if let Some(done) = rec.finished.take() {
        release_connection(world);
        return match done {
            Finished::Closing { closing, summary } => collect(closing, summary),
            Finished::Failed { error, .. } => Err(error),
        };
    }
    let Some(f) = rec.file.take() else {
        return Err(NetDemoError::Idle);
    };
    release_connection(world);
    close_file(f, EndReason::Stopped)
}

/// Starts keeping the last seconds in memory (spec 103 § 7).
///
/// # Errors
/// `net_not_server`, `recorder_busy`, `recorder_view_conflict`, `recorder_view_invalid`, `recorder_config_invalid`.
pub fn start_ring(world: &mut World, config: RingConfig) -> Result<(), NetDemoError> {
    require_server(world)?;
    if world
        .get_resource::<NetRecorder>()
        .is_some_and(|r| r.ring.is_some())
    {
        return Err(NetDemoError::Busy("ring"));
    }
    if !config.seconds.is_finite() || config.seconds <= 0.0 {
        return Err(NetDemoError::ConfigInvalid("seconds must be above 0"));
    }
    if config.max_bytes < 4096 || config.max_bytes > MAX_RING_BYTES {
        return Err(NetDemoError::ConfigInvalid(
            "max_bytes must be at least 4096 and at most 4 GiB",
        ));
    }
    let hz = runtime::tick_hz(world);
    let every = keyframe_ticks(config.keyframe_every_s, hz)?;
    if world
        .get_resource::<NetRecorder>()
        .and_then(|r| r.view)
        .is_some_and(|v| v != config.view)
    {
        return Err(NetDemoError::ViewConflict);
    }
    let window = (f64::from(config.seconds) * hz).ceil() as u32;
    // Before the connection: a ring that cannot be allocated leaves nothing running.
    let ring = ring::Ring::new(config.max_bytes, window, every)?;
    ensure_connection(world, config.view)?;
    world.resource_mut::<NetRecorder>().ring = Some(RingRecording {
        ring,
        keyframe_every_ticks: every,
    });
    Ok(())
}

/// Stops the ring and frees its memory.
///
/// # Errors
/// `ring_idle`.
pub fn stop_ring(world: &mut World) -> Result<RingStatus, NetDemoError> {
    let Some(mut rec) = world.get_resource_mut::<NetRecorder>() else {
        return Err(NetDemoError::RingIdle);
    };
    let Some(r) = rec.ring.take() else {
        return Err(NetDemoError::RingIdle);
    };
    release_connection(world);
    Ok(r.ring.status())
}

/// Writes the ring's newest `seconds` (`None`: all of it) to `path`, atomically. The ring keeps running.
///
/// # Errors
/// `ring_idle`, `ring_empty`, `netdemo_exists`, `netdemo_io`.
pub fn export_ring(
    world: &mut World,
    path: &Path,
    seconds: Option<f32>,
    overwrite: bool,
) -> Result<RecordingSummary, NetDemoError> {
    let _span = tracing::trace_span!("net_recorder_export").entered();
    let Some(rec) = world.get_resource::<NetRecorder>() else {
        return Err(NetDemoError::RingIdle);
    };
    let Some(r) = rec.ring.as_ref() else {
        return Err(NetDemoError::RingIdle);
    };
    let status = r.ring.status();
    let Some(newest) = status.last_tick else {
        return Err(NetDemoError::RingEmpty);
    };
    let from = match seconds {
        Some(s) if s.is_finite() && s >= 0.0 => {
            NetTick(newest.0.saturating_sub((f64::from(s) * rec.hz).ceil() as u32))
        }
        Some(_) => return Err(NetDemoError::ConfigInvalid("seconds must be 0 or above")),
        None => NetTick(0),
    };
    let Some((first, a, b)) = r.ring.slices_from(from) else {
        return Err(NetDemoError::RingEmpty);
    };
    let (ticks, keyframes, last) = ring::count_chunks(a, b);
    let every = r.keyframe_every_ticks;
    let view = rec.view.unwrap_or_default();
    let mut header = make_header(world, every, view, &rec.session_level);
    header.first_tick = first;
    let head = header.encode();
    let mut tail = Vec::with_capacity(format::END_CHUNK_BYTES);
    format::put_end_chunk(
        &mut tail,
        &format::EndSummary {
            tick_chunks: ticks,
            keyframes,
            last_tick: last,
            reason: EndReason::Stopped,
        },
    );
    let bytes = sink::write_atomic(path, overwrite, &[&head, a, b, &tail])?;
    Ok(RecordingSummary {
        path: path.to_path_buf(),
        bytes,
        ticks,
        keyframes,
        first_tick: first,
        last_tick: last,
        reason: EndReason::Stopped,
    })
}

/// What the recorder is doing.
#[must_use]
pub fn recorder_status(world: &World) -> RecorderStatus {
    world
        .get_resource::<NetRecorder>()
        .map(NetRecorder::status)
        .unwrap_or_default()
}

// ---- the hooks of `runtime::server_send` ---------------------------------------------------------

/// Before a send tick's gather: a due keyframe's connection joins, for this gather.
pub(crate) fn before_gather(world: &mut World, tick: NetTick) {
    let Some(mut rec) = world.get_resource_mut::<NetRecorder>() else {
        return;
    };
    let Some(view) = rec.view else {
        return;
    };
    if !rec.keyframe_due(tick) {
        return;
    }
    rec.keyframe_wanted = true;
    rec.force_keyframe = false;
    rec.last_keyframe = Some(tick);
    world
        .resource_mut::<ServerReplication>()
        .add_recorder_conn(KEYFRAME_PEER, view);
}

/// After the gather: the keyframe connection's section becomes the keyframe, and the connection goes.
pub(crate) fn take_keyframe(world: &mut World, tick: NetTick, sections: &mut BTreeMap<PeerId, GatheredConn>) {
    let Some(mut rec) = world.get_resource_mut::<NetRecorder>() else {
        return;
    };
    if !std::mem::take(&mut rec.keyframe_wanted) {
        return;
    }
    let _span = tracing::trace_span!("net_recorder_keyframe").entered();
    if let Some(g) = sections.remove(&KEYFRAME_PEER) {
        let mut w = BitWriter::with_capacity(g.section.bit_len().div_ceil(8) + 16);
        w.write_bits(u64::from(TAG_TICK), 8);
        w.write_varint(u64::from(tick.0));
        w.append(&g.section);
        // No messages: a seek is silent (spec 103 § 8).
        w.write_varint(0);
        rec.set_keyframe(tick, w.as_bytes());
    }
    world
        .resource_mut::<ServerReplication>()
        .remove_conn(KEYFRAME_PEER);
}

/// End of a send tick: this tick's frames for the recorder leave the outbox for the file and the ring. A session that
/// moved to another level ends the file (one file holds one level).
pub(crate) fn after_send(world: &mut World, tick: NetTick) {
    if world
        .get_resource::<NetRecorder>()
        .is_none_or(|r| r.view.is_none())
    {
        return;
    }
    let _span = tracing::trace_span!("net_recorder").entered();
    world.resource_scope(|world, mut rec: Mut<NetRecorder>| {
        let rec = &mut *rec;
        let gone = match rec.view {
            Some(RecorderView::Player(p)) => !world.resource::<NetNode>().has_peer(p),
            _ => false,
        };
        if gone && !rec.view_peer_gone {
            world.resource_mut::<ServerReplication>().freeze_recorder_view();
        }
        rec.view_peer_gone = gone;
        rec.begin_tick(tick);
        world.resource_mut::<NetNode>().outbox.retain(|d| {
            if d.peer == RECORDER_PEER {
                rec.add_frame(d.channel, &d.bytes);
                false
            } else {
                true
            }
        });
        if rec.frames_this_tick == 0 {
            return;
        }
        rec.end_tick(tick);
        if tick.next().0 == u32::MAX {
            rec.finish_file(EndReason::TicksExhausted);
        }
        if let Some(s) = world.get_resource::<NetSession>().and_then(|s| s.server()) {
            rec.follow_session_level(s.level());
        }
    });
    release_connection(world);
}
