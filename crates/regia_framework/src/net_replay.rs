// SPDX-License-Identifier: MPL-2.0
//! The network replay player (spec 103 § 8, `book/agents/net-replay.md`, ADR-3021): plays a `.netdemo` with no server, by
//! feeding its frames to the normal client path, so prefabs, interpolation, despawn timing and message handlers behave
//! as live. Not the local replay system ([`crate::demo_world`]), which simulates again from recorded input.
//!
//! ```no_run
//! # use regia_ecs::prelude::*;
//! # use regia_framework::net_replay::*;
//! # fn f(world: &mut World) -> Result<(), regia_net::recorder::NetDemoError> {
//! open_replay(world, std::path::Path::new("match.netdemo"), ReplayOptions::default())?;
//! # Ok(()) }
//! ```
//!
//! [`NetReplayPlugin`] runs an opened player from the app's real time. The player's own methods take the world, so a
//! test or a tool drives it in fixed steps instead.

use std::io::{Read, Seek};
use std::path::Path;

use regia_app::prelude::*;
use regia_net::message::Channel;
use regia_net::model::{LocalPeer, NetId, NetMode, NetTick};
use regia_net::plugin::NetSet;
use regia_net::recorder::format::{ChunkKind, NetDemoHeader, frames_of};
use regia_net::recorder::playback::{
    is_tick_frame, keyframe_with_messages, registry_has_refs, reset_client_world, restamp_tick_frame,
    state_only_frame, validate_frame,
};
use regia_net::recorder::{NetDemoError, NetDemoReader, NetReplayClock, RECORDER_PEER};
use regia_net::replication::ClientReplication;
use regia_net::runtime::{Datagram, NetEntities, NetNode};
use regia_net::session::{NetSession, PeerId};

use crate::spectator::{SpectatorCamera, SpectatorDrive, SpectatorSet};

/// The slowest and fastest playback.
pub const REPLAY_SPEED_RANGE: (f32, f32) = (1.0 / 16.0, 16.0);
/// Frames fed per apply, so a long seek or a fast play stays under the client's pending limits.
pub const SEEK_FRAMES_PER_APPLY: usize = 2048;
/// Frame bytes fed per apply.
pub const SEEK_BYTES_PER_APPLY: usize = 32 << 20;

/// What the replay's camera does (spec 103 § 8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplayCamera {
    /// [`SpectatorCamera`] flies free.
    #[default]
    Free,
    /// [`SpectatorCamera`] chases this copy: the id survives seeks and the copy's despawn.
    Follow(NetId),
}

/// How to open a replay.
#[derive(Debug, Clone, Copy)]
pub struct ReplayOptions {
    /// Open paused.
    pub paused: bool,
    /// Show proxies through the interpolation clock, on playback time. Off: values apply on arrival.
    pub interpolate: bool,
    /// Runs on every reset after the open (a seek back, a restart, the close), with the header's level: a host that
    /// reloads the level gets back level-bound entities a recording removed (spec 103 § 8.1).
    pub level_restore: Option<fn(&mut World, &str)>,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            paused: false,
            interpolate: true,
            level_restore: None,
        }
    }
}

/// The player's state (`net.replay_state`).
#[derive(Debug, Clone, PartialEq)]
pub struct NetReplayState {
    /// The playhead's whole tick.
    pub tick: u32,
    /// The first recorded tick.
    pub first_tick: u32,
    /// The last recorded tick.
    pub last_tick: u32,
    /// Server ticks per second.
    pub tick_hz: f64,
    /// Seconds from the first tick.
    pub seconds: f64,
    /// Seconds in the recording.
    pub total_seconds: f64,
    /// Whether it is running.
    pub playing: bool,
    /// It reached the last tick.
    pub ended: bool,
    /// A seek is still feeding its segment.
    pub seeking: bool,
    /// The speed.
    pub speed: f32,
    /// Counts seeks: code holding per-copy state starts over when it moves.
    pub generation: u64,
    /// The camera.
    pub camera: ReplayCamera,
    /// The followed copy at the playhead.
    pub follow_entity: Option<Entity>,
    /// Keyframes in the file.
    pub keyframes: usize,
    /// Copies the client path asked the (absent) server to send again.
    pub resync_unanswered: u64,
    /// What stopped playback (`netdemo_*`).
    pub error: Option<String>,
}

/// What a player reads from: a file, or memory in a tool.
pub trait DemoSource: Read + Seek + Send + Sync {}
impl<T: Read + Seek + Send + Sync> DemoSource for T {}

/// An open replay: see the module docs. A resource under [`NetReplayPlugin`]; plain state for a test.
#[derive(Resource)]
pub struct NetReplayPlayer {
    reader: NetDemoReader<Box<dyn DemoSource>>,
    /// In server ticks. Starts one tick before the first, so the first step of a tick feeds the first tick.
    playhead: f64,
    speed: f32,
    playing: bool,
    ended: bool,
    /// The next chunk to read.
    cursor: u64,
    /// The newest tick fed.
    fed: Option<NetTick>,
    started: bool,
    requested_seek: Option<NetTick>,
    seek_target: Option<NetTick>,
    /// A large keyframe's second pass waits for the first to apply: both at once could pass the pending limit.
    second_pass: bool,
    /// Back to the start at the next step, messages and all.
    restart: bool,
    generation: u64,
    camera: ReplayCamera,
    camera_applied: Option<ReplayCamera>,
    payload: Vec<u8>,
    key_payload: Vec<u8>,
    clock_dirty: bool,
    interpolate: bool,
    level_restore: Option<fn(&mut World, &str)>,
    has_refs: bool,
    resync_unanswered: u64,
    error: Option<NetDemoError>,
    saved: (NetMode, Option<PeerId>),
    /// This player's open mark (`NetReplayOpen`).
    token: std::sync::Arc<()>,
}

impl std::fmt::Debug for NetReplayPlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetReplayPlayer")
            .field("playhead", &self.playhead)
            .field("playing", &self.playing)
            .field("level", &self.reader.header().level)
            .finish()
    }
}

/// The open player of a world (ADR-8762): `token` lives as long as the player, `saved` is the world's mode and local
/// peer from before the replay. A player dropped without [`NetReplayPlayer::close`] leaves a dead token, and the next
/// open restores from `saved` rather than the replay's own client mode — but only while the world never left the dropped
/// replay (`LocalPeer == RECORDER_PEER`); a session since then changed the mode, and the next open saves that instead
/// (ADR-8850).
#[derive(Resource)]
struct NetReplayOpen {
    token: std::sync::Weak<()>,
    saved: (NetMode, Option<PeerId>),
}

fn session_runs(world: &World) -> bool {
    world
        .get_resource::<NetSession>()
        .is_some_and(|s| s.server().is_some() || s.client().is_some())
}

/// Opens `path` and makes it this world's running replay: the resource [`NetReplayPlugin`] plays.
///
/// # Errors
/// As [`NetReplayPlayer::open`].
pub fn open_replay(world: &mut World, path: &Path, options: ReplayOptions) -> Result<(), NetDemoError> {
    close_replay(world);
    let player = NetReplayPlayer::open(world, path, options)?;
    world.insert_resource(player);
    Ok(())
}

/// Closes this world's running replay, if any.
pub fn close_replay(world: &mut World) {
    if let Some(player) = world.remove_resource::<NetReplayPlayer>() {
        player.close(world);
    }
}

impl NetReplayPlayer {
    /// Opens a file for this world (spec 103 § 8). The whole file is checked here.
    ///
    /// # Errors
    /// `replay_already_open` (another player of this world is open: close it first, ADR-8762), `netdemo_session_active`
    /// (a session runs in this world), `netdemo_protocol_mismatch` (this world registered another protocol), or what
    /// the reader reports: `netdemo_not_a_demo`, `netdemo_version`, `netdemo_truncated`,
    /// `netdemo_corrupt`, `netdemo_io`.
    pub fn open(world: &mut World, path: &Path, options: ReplayOptions) -> Result<Self, NetDemoError> {
        let file =
            std::fs::File::open(path).map_err(|e| NetDemoError::Io(format!("{}: {e}", path.display())))?;
        Self::open_source(world, std::io::BufReader::new(file), options)
    }

    /// [`Self::open`] from any source.
    ///
    /// # Errors
    /// As [`Self::open`].
    pub fn open_source(
        world: &mut World,
        source: impl DemoSource + 'static,
        options: ReplayOptions,
    ) -> Result<Self, NetDemoError> {
        // ADR-8762: a second player would save the first one's replay mode as the mode to restore.
        let open_before = world
            .get_resource::<NetReplayOpen>()
            .map(|o| (o.token.strong_count() > 0, o.saved));
        if open_before.is_some_and(|(alive, _)| alive) {
            return Err(NetDemoError::ReplayOpen);
        }
        let boxed: Box<dyn DemoSource> = Box::new(source);
        let reader = NetDemoReader::open(boxed)?;
        regia_net::runtime::ensure_resources(world);
        // Hosting or joining, by session or by mode, or recording: the reset below would take that world's entities.
        let serving = matches!(
            *world.resource::<NetMode>(),
            NetMode::DedicatedServer | NetMode::ListenServer
        );
        let recording = world
            .get_resource::<regia_net::recorder::NetRecorder>()
            .is_some_and(|r| r.status().view.is_some());
        if serving || recording || session_runs(world) || !world.resource::<NetNode>().peers().is_empty() {
            return Err(NetDemoError::SessionActive);
        }
        reader.header().check_world(world)?;
        // A player dropped without its close left the world in replay mode: the mode before it is the one to restore.
        // ADR-8850: a dead mark's `saved` is reused only while the world never left that dropped
        // replay (`LocalPeer == RECORDER_PEER`). A session since then changed the mode and peer;
        // this replay's close must restore those, not the ones from before the drop.
        let still_the_dropped_replay = world
            .get_resource::<LocalPeer>()
            .is_some_and(|p| p.0 == Some(RECORDER_PEER));
        let saved = open_before.map_or_else(
            || {
                (
                    *world.resource::<NetMode>(),
                    world.get_resource::<LocalPeer>().and_then(|p| p.0),
                )
            },
            |(_, saved)| {
                if still_the_dropped_replay {
                    saved
                } else {
                    (
                        *world.resource::<NetMode>(),
                        world.get_resource::<LocalPeer>().and_then(|p| p.0),
                    )
                }
            },
        );
        let token = std::sync::Arc::new(());
        world.insert_resource(NetReplayOpen {
            token: std::sync::Arc::downgrade(&token),
            saved,
        });
        world.insert_resource(NetMode::Client);
        world.insert_resource(LocalPeer(Some(RECORDER_PEER)));
        let header = reader.header().clone();
        reset_client_world(world);
        if options.interpolate {
            world.insert_resource(NetReplayClock);
            let send_every = f64::from(header.send_every_ticks);
            match world.get_resource_mut::<regia_net::interp::NetInterpolation>() {
                Some(mut ni) => ni.clock.set_server_rate(header.tick_hz, send_every),
                None => regia_net::interp::install_interpolation(world, header.tick_hz, send_every),
            }
        }
        let has_refs = registry_has_refs(world);
        let cursor = reader.first_chunk();
        let first = reader.first_tick();
        Ok(Self {
            reader,
            playhead: f64::from(first.0) - 1.0,
            speed: 1.0,
            playing: !options.paused,
            ended: false,
            cursor,
            fed: None,
            started: false,
            requested_seek: None,
            seek_target: None,
            second_pass: false,
            restart: false,
            generation: 0,
            camera: ReplayCamera::Free,
            camera_applied: None,
            payload: Vec::new(),
            key_payload: Vec::new(),
            clock_dirty: true,
            interpolate: options.interpolate,
            level_restore: options.level_restore,
            has_refs,
            resync_unanswered: 0,
            error: None,
            saved,
            token,
        })
    }

    /// Ends the replay: the copies go, a host's `level_restore` runs, and the world's mode returns to what it was. A
    /// session started meanwhile owns the world by then, so it keeps its entities and its mode.
    pub fn close(self, world: &mut World) {
        if world
            .get_resource::<NetReplayOpen>()
            .is_some_and(|o| std::ptr::eq(o.token.as_ptr(), std::sync::Arc::as_ptr(&self.token)))
        {
            world.remove_resource::<NetReplayOpen>();
        }
        world.remove_resource::<NetReplayClock>();
        if session_runs(world) {
            return;
        }
        reset_client_world(world);
        if let Some(restore) = self.level_restore {
            restore(world, &self.reader.header().level.clone());
        }
        world.insert_resource(self.saved.0);
        world.insert_resource(LocalPeer(self.saved.1));
    }

    /// The file's header.
    #[must_use]
    pub fn header(&self) -> &NetDemoHeader {
        self.reader.header()
    }

    /// Runs. After the end, starts over from the first tick, as the open did: its messages are delivered again.
    pub fn play(&mut self) {
        if self.ended {
            self.restart = true;
            self.ended = false;
        }
        self.playing = true;
    }

    /// Stops the playhead. A pending seek still lands.
    pub fn pause(&mut self) {
        self.playing = false;
    }

    /// Sets the speed, clamped to [`REPLAY_SPEED_RANGE`] (a non-finite value is ignored).
    pub fn set_speed(&mut self, speed: f32) {
        if speed.is_finite() {
            self.speed = speed.clamp(REPLAY_SPEED_RANGE.0, REPLAY_SPEED_RANGE.1);
        }
    }

    /// Moves to `tick`, silently (spec 103 § 8): state only, no messages. It lands on the next step.
    ///
    /// # Errors
    /// `replay_seek_range`.
    pub fn seek(&mut self, tick: u32) -> Result<(), NetDemoError> {
        let (first, last) = (self.reader.first_tick().0, self.reader.last_tick().0);
        if tick < first || tick > last {
            return Err(NetDemoError::SeekRange { tick, first, last });
        }
        self.requested_seek = Some(NetTick(tick));
        Ok(())
    }

    /// Moves to `seconds` from the first tick.
    ///
    /// # Errors
    /// `replay_seek_range`.
    pub fn seek_seconds(&mut self, seconds: f64) -> Result<(), NetDemoError> {
        let (first, last) = (self.reader.first_tick().0, self.reader.last_tick().0);
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(NetDemoError::SeekRange {
                tick: first,
                first,
                last,
            });
        }
        let ticks = (seconds * self.reader.header().tick_hz)
            .round()
            .min(f64::from(u32::MAX)) as u32;
        self.seek(first.saturating_add(ticks))
    }

    /// Sets the camera's mode.
    pub fn set_camera(&mut self, camera: ReplayCamera) {
        self.camera = camera;
    }

    /// The state.
    #[must_use]
    pub fn state(&self, world: &World) -> NetReplayState {
        let hz = self.reader.header().tick_hz;
        let (first, last) = (self.reader.first_tick().0, self.reader.last_tick().0);
        let tick = self.playhead.max(f64::from(first)).min(f64::from(last)) as u32;
        NetReplayState {
            tick,
            first_tick: first,
            last_tick: last,
            tick_hz: hz,
            seconds: f64::from(tick - first) / hz,
            total_seconds: f64::from(last - first) / hz,
            playing: self.playing,
            ended: self.ended,
            seeking: self.seek_target.is_some() || self.requested_seek.is_some(),
            speed: self.speed,
            generation: self.generation,
            camera: self.camera,
            follow_entity: match self.camera {
                ReplayCamera::Follow(id) => world.get_resource::<NetEntities>().and_then(|m| m.entity(id)),
                ReplayCamera::Free => None,
            },
            keyframes: self.reader.keyframes().len(),
            resync_unanswered: self.resync_unanswered,
            error: self.error.as_ref().map(ToString::to_string),
        }
    }

    /// What stopped playback.
    #[must_use]
    pub fn error(&self) -> Option<&NetDemoError> {
        self.error.as_ref()
    }

    /// One frame of real time: moves the playhead by `dt_seconds × speed` and feeds what it passed.
    pub fn advance(&mut self, world: &mut World, dt_seconds: f64) {
        let ticks = dt_seconds * self.reader.header().tick_hz * f64::from(self.speed);
        self.advance_ticks(world, ticks);
    }

    /// Moves the playhead by `ticks` server ticks and feeds what it passed: the deterministic form of
    /// [`Self::advance`]. A pause holds the playhead, a seek still lands.
    pub fn advance_ticks(&mut self, world: &mut World, ticks: f64) {
        if self.error.is_some() {
            return;
        }
        if let Err(e) = self.step(world, ticks) {
            tracing::warn!(target: "net", error = %e, "net.replay_stopped");
            self.error = Some(e);
            self.playing = false;
        }
    }

    fn step(&mut self, world: &mut World, ticks: f64) -> Result<(), NetDemoError> {
        // A session started in this world: it owns the client path now (spec 103 § 8).
        if session_runs(world) {
            return Err(NetDemoError::SessionActive);
        }
        if std::mem::take(&mut self.restart) {
            self.begin_restart(world);
        }
        if let Some(target) = self.requested_seek.take() {
            self.begin_seek(world, target)?;
        }
        if self.seek_target.is_some() {
            self.stamp_arrival(world);
            return self.feed_seek(world);
        }
        if !self.playing {
            return Ok(());
        }
        let last = f64::from(self.reader.last_tick().0);
        self.playhead = (self.playhead + ticks.max(0.0)).min(last);
        self.stamp_arrival(world);
        self.feed_linear(world)?;
        if self.playhead >= last && self.fed.is_some_and(|f| f >= self.reader.last_tick()) {
            self.ended = true;
            self.playing = false;
        }
        Ok(())
    }

    fn ms_per_tick(&self) -> f64 {
        1000.0 / self.reader.header().tick_hz
    }

    fn playback_ms(&self) -> f64 {
        self.playhead.max(0.0) * self.ms_per_tick()
    }

    fn stamp_arrival(&self, world: &mut World) {
        if self.interpolate
            && let Some(mut ni) = world.get_resource_mut::<regia_net::interp::NetInterpolation>()
        {
            ni.now_ms = self.playback_ms();
        }
    }

    /// Whether another frame fits the client's pending limits right now.
    fn room(&self, world: &World, frames: usize, bytes: usize) -> bool {
        let node = world.resource::<NetNode>();
        let pending = node.pending_counts().0 + node.inbox.len() + frames;
        let held = node.pending_frame_bytes() + bytes;
        frames == 0 || (pending < SEEK_FRAMES_PER_APPLY && held < SEEK_BYTES_PER_APPLY)
    }

    fn push(world: &mut World, channel: Channel, bytes: Vec<u8>) {
        world.resource_mut::<NetNode>().inbox.push(Datagram {
            peer: PeerId::SERVER,
            channel,
            bytes,
        });
    }

    /// Linear play: the frames of every tick the playhead passed, as they were recorded.
    fn feed_linear(&mut self, world: &mut World) -> Result<(), NetDemoError> {
        let target = (self.playhead + 1e-6).floor();
        if target < 0.0 {
            return Ok(());
        }
        let target = target.min(f64::from(u32::MAX)) as u32;
        if target < self.reader.first_tick().0 {
            return Ok(());
        }
        if !self.started && !self.feed_first(world)? {
            return Ok(());
        }
        let (mut frames, mut bytes) = (0, 0);
        let end = self.reader.file_len();
        while self.cursor < end && self.room(world, frames, bytes) {
            let head = self.reader.read_head(self.cursor)?;
            match head.kind {
                ChunkKind::End => break,
                ChunkKind::Keyframe => {
                    self.cursor = head.next;
                    continue;
                }
                ChunkKind::Tick => {}
            }
            if self.fed.is_some_and(|f| head.tick <= f) {
                self.cursor = head.next;
                continue;
            }
            if head.tick.0 > target {
                break;
            }
            let mut payload = std::mem::take(&mut self.payload);
            let read = self.reader.read_chunk(self.cursor, &mut payload);
            let fed = read.and_then(|_| {
                let mut count = (0usize, 0usize);
                for frame in frames_of(&payload) {
                    let (channel, frame) = frame?;
                    validate_frame(frame)?;
                    count.0 += 1;
                    count.1 += frame.len();
                    Self::push(world, channel, frame.to_vec());
                }
                Ok(count)
            });
            self.payload = payload;
            let (n, b) = fed?;
            frames += n;
            bytes += b;
            self.fed = Some(head.tick);
            self.cursor = head.next;
        }
        Ok(())
    }

    /// Back to where an open leaves it: the next step feeds the first tick with its messages.
    fn begin_restart(&mut self, world: &mut World) {
        reset_client_world(world);
        if let Some(restore) = self.level_restore {
            restore(world, &self.reader.header().level.clone());
        }
        self.generation += 1;
        self.clock_dirty = true;
        self.playhead = f64::from(self.reader.first_tick().0) - 1.0;
        self.cursor = self.reader.first_chunk();
        self.fed = None;
        self.started = false;
        self.seek_target = None;
        self.second_pass = false;
    }

    /// The first recorded tick: its keyframe (the connection may have run before the file) with that tick's messages.
    /// `false` while a large keyframe's first pass still has to apply.
    fn feed_first(&mut self, world: &mut World) -> Result<bool, NetDemoError> {
        let mut key = std::mem::take(&mut self.key_payload);
        let result = self.feed_first_with(world, &mut key);
        self.key_payload = key;
        result
    }

    fn feed_first_with(&mut self, world: &mut World, key: &mut Vec<u8>) -> Result<bool, NetDemoError> {
        let head = self.reader.read_chunk(self.cursor, key)?;
        validate_frame(key)?;
        // A connection older than the file gains no copy at this keyframe, so nothing heals a reference to a copy later
        // in the list: it goes twice, as a seek's does (spec 103 § 3.6).
        if self.has_refs && head.tick.0 > 0 && !self.second_pass {
            let first = restamp_tick_frame(key, NetTick(head.tick.0 - 1))?;
            Self::push(world, Channel::ReliableOrdered, first);
            self.second_pass = true;
            if key.len().saturating_mul(2) >= SEEK_BYTES_PER_APPLY {
                return Ok(false);
            }
        } else if self.second_pass {
            let node = world.resource::<NetNode>();
            if node.pending_frame_bytes() > 0 || !node.inbox.is_empty() {
                return Ok(false);
            }
        }
        self.second_pass = false;
        self.started = true;
        self.cursor = head.next;
        let tick_head = self.reader.read_chunk(self.cursor, &mut self.payload)?;
        if tick_head.kind != ChunkKind::Tick || tick_head.tick != head.tick {
            // A keyframe without its tick's chunk: the state alone.
            Self::push(world, Channel::ReliableOrdered, key.clone());
            self.fed = Some(head.tick);
            return Ok(true);
        }
        for frame in frames_of(&self.payload) {
            let (channel, frame) = frame?;
            validate_frame(frame)?;
            let bytes = if is_tick_frame(frame) {
                keyframe_with_messages(key, frame)?
            } else {
                frame.to_vec()
            };
            Self::push(world, channel, bytes);
        }
        self.cursor = tick_head.next;
        self.fed = Some(head.tick);
        Ok(true)
    }

    fn begin_seek(&mut self, world: &mut World, target: NetTick) -> Result<(), NetDemoError> {
        let _span = tracing::trace_span!("net_replay_seek").entered();
        let Some(key) = self.reader.keyframe_at_or_before(target) else {
            return Err(NetDemoError::Corrupt(
                "no keyframe at or before the target".to_string(),
            ));
        };
        self.generation += 1;
        self.clock_dirty = true;
        self.ended = false;
        self.playhead = f64::from(target.0);
        self.seek_target = Some(target);
        self.second_pass = false;
        // Ahead in the same segment: the state the world holds is the way there.
        if self.fed.is_some_and(|f| f <= target && key.tick <= f) {
            return Ok(());
        }
        reset_client_world(world);
        if let Some(restore) = self.level_restore {
            restore(world, &self.reader.header().level.clone());
        }
        let head = self.reader.read_chunk(key.offset, &mut self.key_payload)?;
        validate_frame(&self.key_payload)?;
        // A keyframe that can hold references goes twice: the first pass makes every copy, the second resolves what
        // pointed at a copy later in the list (spec 103 § 3.6).
        if self.has_refs && head.tick.0 > 0 {
            let first = restamp_tick_frame(&self.key_payload, NetTick(head.tick.0 - 1))?;
            Self::push(world, Channel::ReliableOrdered, first);
            self.second_pass = self.key_payload.len().saturating_mul(2) >= SEEK_BYTES_PER_APPLY;
        }
        if !self.second_pass {
            Self::push(world, Channel::ReliableOrdered, self.key_payload.clone());
        }
        self.started = true;
        self.fed = Some(head.tick);
        self.cursor = head.next;
        Ok(())
    }

    fn feed_seek(&mut self, world: &mut World) -> Result<(), NetDemoError> {
        let Some(target) = self.seek_target else {
            return Ok(());
        };
        let _span = tracing::trace_span!("net_replay_seek").entered();
        if self.second_pass {
            let node = world.resource::<NetNode>();
            if node.pending_frame_bytes() > 0 || !node.inbox.is_empty() {
                return Ok(());
            }
            self.second_pass = false;
            Self::push(world, Channel::ReliableOrdered, self.key_payload.clone());
        }
        let (mut frames, mut bytes) = (0, 0);
        let end = self.reader.file_len();
        while self.cursor < end {
            if !self.room(world, frames, bytes) {
                return Ok(());
            }
            let head = self.reader.read_head(self.cursor)?;
            match head.kind {
                ChunkKind::End => break,
                ChunkKind::Keyframe => {
                    self.cursor = head.next;
                    continue;
                }
                ChunkKind::Tick => {}
            }
            if self.fed.is_some_and(|f| head.tick <= f) {
                self.cursor = head.next;
                continue;
            }
            if head.tick > target {
                break;
            }
            let mut payload = std::mem::take(&mut self.payload);
            let read = self.reader.read_chunk(self.cursor, &mut payload);
            let fed = read.and_then(|_| {
                let mut count = (0usize, 0usize);
                for frame in frames_of(&payload) {
                    let (channel, frame) = frame?;
                    // Messages stay behind: a seek is silent.
                    if let Some(state) = state_only_frame(frame)? {
                        validate_frame(&state)?;
                        count.0 += 1;
                        count.1 += state.len();
                        Self::push(world, channel, state);
                    }
                }
                Ok(count)
            });
            self.payload = payload;
            let (n, b) = fed?;
            frames += n;
            bytes += b;
            self.fed = Some(head.tick);
            self.cursor = head.next;
        }
        self.seek_target = None;
        Ok(())
    }

    /// After `NetSet::Apply`: what the client path asked the absent server to send again is counted and dropped.
    pub fn after_apply(&mut self, world: &mut World) {
        let mut client = world.resource_mut::<ClientReplication>();
        if !client.resync.is_empty() {
            self.resync_unanswered += client.resync.len() as u64;
            client.resync.clear();
        }
    }

    /// After `NetSet::Send`: nobody receives what the client path sent. A session's traffic is left alone.
    pub fn after_send(&mut self, world: &mut World) {
        if session_runs(world) {
            return;
        }
        world.resource_mut::<NetNode>().outbox.clear();
        let closed = world.resource_mut::<NetNode>().closed.take();
        if let Some(reason) = closed
            && self.error.is_none()
        {
            self.error = Some(NetDemoError::Corrupt(format!(
                "the client path refused a frame: {}",
                reason.as_str()
            )));
            self.playing = false;
        }
    }

    /// Shows proxies at playback time (`PostUpdate`).
    pub fn interpolate(&mut self, world: &mut World) {
        if !self.interpolate {
            return;
        }
        let now = self.playback_ms();
        // After a seek the arrivals were a burst: the clock starts from where the playhead landed.
        if self.clock_dirty
            && self.seek_target.is_none()
            && world.resource::<NetNode>().pending_counts().0 == 0
            && world.resource::<NetNode>().inbox.is_empty()
            && let Some(fed) = self.fed
            && let Some(mut ni) = world.get_resource_mut::<regia_net::interp::NetInterpolation>()
        {
            ni.clock.restart();
            ni.clock.on_state(u64::from(fed.0), now);
            self.clock_dirty = false;
        }
        regia_net::interp::net_interpolate(world, now);
    }

    fn drive_camera(&mut self, world: &mut World) {
        let follow = match self.camera {
            ReplayCamera::Follow(id) => world.get_resource::<NetEntities>().and_then(|m| m.entity(id)),
            ReplayCamera::Free => None,
        };
        if self.camera_applied != Some(self.camera) {
            self.camera_applied = Some(self.camera);
            if let Some(mut camera) = world.get_resource_mut::<SpectatorCamera>() {
                match self.camera {
                    ReplayCamera::Free => camera.free_fly(),
                    ReplayCamera::Follow(_) => camera.follow(),
                }
            }
            if let Some(mut drive) = world.get_resource_mut::<SpectatorDrive>() {
                drive.follow = None;
                drive.lost = None;
            }
        }
        if let (Some(entity), Some(mut drive)) = (follow, world.get_resource_mut::<SpectatorDrive>())
            && drive.follow != Some(entity)
        {
            drive.follow = Some(entity);
            drive.lost = None;
        }
    }
}

/// Plays an opened [`NetReplayPlayer`] from the app's real time. Does nothing while none is open.
#[derive(Debug, Clone, Copy, Default)]
pub struct NetReplayPlugin;

fn with_player(world: &mut World, f: impl FnOnce(&mut NetReplayPlayer, &mut World)) {
    if world.contains_resource::<NetReplayPlayer>() {
        world.resource_scope(|world, mut player: Mut<NetReplayPlayer>| f(&mut player, world));
    }
}

fn replay_feed(world: &mut World) {
    let dt = world
        .get_resource::<RealTime>()
        .map_or(0.0, |t| t.delta().as_secs_f64());
    with_player(world, |player, world| player.advance(world, dt));
}

fn replay_after_apply(world: &mut World) {
    with_player(world, NetReplayPlayer::after_apply);
}

fn replay_flush(world: &mut World) {
    with_player(world, NetReplayPlayer::after_send);
}

fn replay_interpolate(world: &mut World) {
    with_player(world, NetReplayPlayer::interpolate);
}

fn replay_camera(world: &mut World) {
    with_player(world, NetReplayPlayer::drive_camera);
}

impl Plugin for NetReplayPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PreUpdate, replay_feed.before(NetSet::Receive))
            .add_systems(
                FixedUpdate,
                replay_after_apply.after(NetSet::Apply).before(NetSet::Predict),
            )
            .add_systems(
                PostUpdate,
                (
                    replay_flush.in_set(NetSet::Flush),
                    replay_interpolate.in_set(NetSet::Interpolate),
                ),
            )
            .add_systems(PostUpdate, replay_camera.before(SpectatorSet));
    }
}

/// The player's lookups the agent uses without taking the resource.
#[must_use]
pub fn replay_state(world: &World) -> Option<NetReplayState> {
    world.get_resource::<NetReplayPlayer>().map(|p| p.state(world))
}
