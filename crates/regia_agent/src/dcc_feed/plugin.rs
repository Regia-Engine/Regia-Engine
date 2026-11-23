// SPDX-License-Identifier: MPL-2.0
//! The ECS side of the feed (`specs/60` § 3.2, § 3.4): resources, messages and the `PreUpdate`
//! systems that take drained messages from the core and keep entities in step with items.
//!
//! Ingest and apply are exclusive systems. They cost one atomic load while no session exists.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use regia_animation::{FeedPoseBuffer, Skeleton};
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_render::{CpuMaterial, CpuMesh};

use super::core::{CoreError, FeedCore, TickOutput};
use super::mesh::{FeedMeshData, split_parts};
use super::net::{self, Listeners, lock};
use super::playout::PlayoutBuffer;
use super::registry::{ItemState, ItemTable};
use super::session::{Phase, SessionId};
use super::settings::DccFeedSettings;
use super::wire::{ClipPush, FeedKind, MeshUpdate, WireError};

/// Default step of the feed clock while a replay runs and the host set none (60 Hz).
const REPLAY_STEP_S: f64 = 1.0 / 60.0;
/// Most mesh swaps per frame (spec § 3.4).
pub const MAX_MESH_SWAPS_PER_FRAME: usize = 2;
/// Most mesh bytes swapped per frame (spec § 3.4); one mesh above it still goes through alone.
pub const MAX_MESH_SWAP_BYTES: usize = 8 << 20;

/// The component on every feed-owned entity (spec § 2.3).
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component)]
pub struct FeedItem {
    /// The sender's stable key.
    pub key: String,
    /// The stable id from the key (reflection has no `Guid` yet).
    pub guid: u128,
    /// What the item drives.
    pub kind: FeedKind,
    /// Owning session; transient.
    #[reflect(ignore)]
    pub session: Option<SessionId>,
    /// Current state.
    pub state: ItemState,
    /// True until baked.
    pub owned: bool,
    /// Frames applied.
    pub frames_applied: u64,
    /// Last applied `frame_id`.
    pub last_frame_id: u64,
    /// Last applied `source_time_ns`.
    pub last_source_time_ns: u64,
}

/// A hidden child holding one extra material's triangles of a mesh item. Has no guid and is never
/// saved; picking and selection see the item entity.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedMeshPart {
    /// The slot this part draws.
    pub slot: u32,
}

/// The newest values of a `generic` item (spec § 2).
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct FeedChannels {
    /// Channel names from the declaration.
    pub names: Vec<String>,
    /// Values, index-matched with `names`.
    pub values: Vec<f32>,
}

/// A session changed state.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedSessionChanged {
    /// The session.
    pub session: SessionId,
    /// The state it left (`None`: it is new).
    pub from: Option<Phase>,
    /// The state it entered.
    pub to: Phase,
}

/// What happened to an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedItemChange {
    /// Declared.
    Added,
    /// Frames applied.
    Updated,
    /// No frame for `stall_after_ms`.
    Stalled,
    /// Its session closed.
    Detached,
    /// Baked into ordinary content.
    Baked,
    /// Deleted or purged.
    Removed,
}

/// An item changed.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedItemChanged {
    /// The item.
    pub guid: Guid,
    /// Its entity, when it still has one.
    pub entity: Option<Entity>,
    /// What changed.
    pub change: FeedItemChange,
}

/// A non-fatal problem worth showing (also listed by `dcc_feed.status`).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct DccFeedWarning {
    /// The session.
    pub session: SessionId,
    /// Lowercase code.
    pub code: String,
    /// Human-readable text.
    pub text: String,
}

/// The feed's stages in `PreUpdate`.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DccFeedSet {
    /// Drains the inbox and returns credits.
    Ingest,
    /// Spawns and updates entities and runs delayed playout.
    Apply,
    /// Swaps finished mesh builds.
    MeshSwap,
}

/// The feed's clock. Live it follows real time, so a paused game still shows the artist's edits;
/// a replay, or a host that sets [`Self::fixed_step_s`], steps it per tick, which makes stall
/// timers and delayed playout deterministic (spec § 7).
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq)]
pub struct DccFeedClock {
    /// Seconds per tick; `None` reads the frame's real delta.
    pub fixed_step_s: Option<f64>,
    /// Engine seconds so far.
    pub now_s: f64,
}

/// The server: listeners, the shared core and the project directory.
#[derive(Resource)]
pub struct DccFeedServer {
    core: Arc<Mutex<FeedCore>>,
    live: Arc<AtomicUsize>,
    listeners: Option<Listeners>,
    project_dir: Option<PathBuf>,
    /// Where the socket, token and endpoint live: the project's, or the app's data directory on
    /// a device with no project (Android reads the token through `adb`).
    state_dir: Option<PathBuf>,
}

impl std::fmt::Debug for DccFeedServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DccFeedServer")
            .field("project_dir", &self.project_dir)
            .field("listening", &self.listeners.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for DccFeedServer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl DccFeedServer {
    /// A server that is not listening yet. The token persists in the project's feed directory;
    /// without a project it lives in memory.
    #[must_use]
    pub fn new(settings: &DccFeedSettings, project_dir: Option<PathBuf>) -> Self {
        let state_dir = project_dir.as_deref().map(net::feed_dir);
        Self::with_state_dir(settings, project_dir, state_dir)
    }

    /// [`Self::new`] with the socket, token and endpoint files in `state_dir`.
    #[must_use]
    pub fn with_state_dir(
        settings: &DccFeedSettings,
        project_dir: Option<PathBuf>,
        state_dir: Option<PathBuf>,
    ) -> Self {
        let token = state_dir
            .as_deref()
            .map(|dir| dir.join("token"))
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
            .unwrap_or_else(net::new_token);
        let core = FeedCore::new(settings, token);
        let live = core.live_counter();
        Self {
            core: Arc::new(Mutex::new(core)),
            live,
            listeners: None,
            project_dir,
            state_dir,
        }
    }

    /// The shared core.
    #[must_use]
    pub fn core(&self) -> &Arc<Mutex<FeedCore>> {
        &self.core
    }

    /// The project directory, when there is one.
    #[must_use]
    pub fn project_dir(&self) -> Option<&Path> {
        self.project_dir.as_deref()
    }

    /// True while any session record exists: the one atomic an idle server costs.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Relaxed) > 0
    }

    /// The open listeners.
    #[must_use]
    pub fn listeners(&self) -> &[net::ListenerInfo] {
        self.listeners.as_ref().map_or(&[], |l| l.info.as_slice())
    }

    /// Opens the listeners of `settings`. Does nothing when already listening.
    ///
    /// # Errors
    ///
    /// A message for a bad listener set or a bind failure.
    pub fn start(&mut self, settings: &DccFeedSettings) -> Result<(), String> {
        if self.listeners.is_some() {
            return Ok(());
        }
        self.listeners = Some(net::start(settings, &self.core, self.state_dir.as_deref())?);
        Ok(())
    }

    /// Closes the listeners and cuts every connection.
    pub fn stop(&mut self) {
        self.listeners = None;
        let mut core = lock(&self.core);
        let ids: Vec<SessionId> = core.sessions.iter().map(|s| s.id).collect();
        for id in ids {
            let now = core.now_s;
            core.sessions.begin_close(id, "listener stopped", now);
        }
        core.shutdown_all();
    }

    /// Replaces the session token, on disk too. Open sessions are not affected.
    ///
    /// # Errors
    ///
    /// The write failure.
    pub fn rotate_token(&mut self) -> Result<(), CoreError> {
        let token = net::new_token();
        if let Some(dir) = &self.state_dir {
            net::write_endpoint(dir, self.listeners(), &token)
                .map_err(|e| CoreError::internal(format!("cannot write the token: {e}")))?;
        }
        lock(&self.core).set_token(token);
        Ok(())
    }
}

/// One transient texture (`feed://textures/<name>`).
#[derive(Debug, Clone)]
pub struct FeedTexture {
    /// Spec § 4.4: 0 sRGB, 1 linear, 2 normal map.
    pub colorspace: u8,
    /// PNG or JPEG bytes when the sender pushed those (what a bake embeds).
    pub encoded: Option<(u8, Arc<Vec<u8>>)>,
    /// Decoded pixels, when an engine decoder handled them.
    pub decoded: Option<Arc<regia_render::LabTexture>>,
    /// Bytes charged against `max_transient_texture_bytes`.
    pub bytes: u64,
    /// Tick of the last use, for the least-recently-used eviction.
    pub last_used: u64,
}

/// A finished build: the geometry and the render mesh of each slot, made off the main thread, so a
/// swap only assigns them (spec § 10).
#[derive(Debug, Clone)]
pub(super) struct BuiltMesh {
    pub(super) data: Arc<FeedMeshData>,
    pub(super) parts: Vec<(u32, Arc<CpuMesh>)>,
}

impl BuiltMesh {
    pub(super) fn new(data: Arc<FeedMeshData>) -> Self {
        let parts = split_parts(&data);
        Self { data, parts }
    }
}

/// A running mesh build's result slot.
pub(super) type BuildSlot = Arc<Mutex<Option<Result<BuiltMesh, WireError>>>>;

/// Geometry state of one mesh item.
#[derive(Debug, Default)]
pub struct MeshState {
    /// The newest finished geometry.
    pub data: Arc<FeedMeshData>,
    /// Vertex count once every queued update is done (checks vertex-only updates).
    pub(super) expected_vertices: u32,
    pub(super) building: Option<BuildSlot>,
    pub(super) next: Option<(MeshUpdate, f32)>,
    pub(super) ready: Option<BuiltMesh>,
    pub(super) parts: Vec<Entity>,
    pub(super) joints: Vec<Entity>,
    pub(super) materials: Vec<Arc<CpuMaterial>>,
}

/// A skeleton item's static data.
#[derive(Debug, Clone)]
pub struct FeedSkeleton {
    /// Joint names, in the wire's order.
    pub names: Arc<Vec<String>>,
    /// The asset a skinned mesh binds to, bind pose and inverse bind matrices included.
    pub asset: Arc<Skeleton>,
}

/// A pushed clip and the item it binds to.
#[derive(Debug, Clone)]
pub struct FeedClip {
    /// The clip as sent.
    pub clip: ClipPush,
    /// The item of its `bind_handle`.
    pub bound: Option<Guid>,
}

/// The item table and the data items carry (spec § 3.2).
#[derive(Resource, Debug, Default)]
pub struct DccFeedRegistry {
    /// Items.
    pub table: ItemTable,
    /// Mesh items' geometry.
    pub meshes: BTreeMap<Guid, MeshState>,
    /// Static data of skeleton items.
    pub skeletons: BTreeMap<Guid, FeedSkeleton>,
    /// Transient textures by name.
    pub textures: BTreeMap<String, FeedTexture>,
    /// Transient clips by name.
    pub clips: BTreeMap<String, FeedClip>,
    pub(super) buffers: BTreeMap<(Guid, u32), PlayoutBuffer>,
    pub(super) anchors: HashMap<SessionId, f64>,
    pub(super) pending: TickOutput,
    pub(super) phases: HashMap<SessionId, Phase>,
}

/// Adds the feed to an app. [`crate::AgentPlugin`] adds it when the `dcc-feed` feature is on;
/// the settings load from the project's `Regia.toml`, and nothing listens unless `enabled`.
#[derive(Debug, Default, Clone)]
pub struct DccFeedPlugin {
    /// The project directory (default: the agent's, resolved at `finish`).
    pub project_dir: Option<PathBuf>,
}

impl Plugin for DccFeedPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<FeedItem>()
            .register_type::<FeedChannels>()
            .init_resource::<DccFeedRegistry>()
            .init_resource::<DccFeedClock>()
            .init_resource::<FeedPoseBuffer>()
            .add_message::<FeedSessionChanged>()
            .add_message::<FeedItemChanged>()
            .add_message::<DccFeedWarning>()
            .configure_sets(
                PreUpdate,
                (DccFeedSet::Ingest, DccFeedSet::Apply, DccFeedSet::MeshSwap).chain(),
            )
            .add_systems(
                PreUpdate,
                (
                    ingest.in_set(DccFeedSet::Ingest),
                    super::apply::apply.in_set(DccFeedSet::Apply),
                    super::apply::mesh_swap.in_set(DccFeedSet::MeshSwap),
                    super::apply::resolve_attachments.after(DccFeedSet::Apply),
                ),
            );
    }

    fn finish(&self, app: &mut App) {
        let project = self.project_dir.clone().or_else(|| {
            let host = app
                .world()
                .get_resource::<crate::AgentSettings>()
                .and_then(|s| s.project_dir.clone());
            crate::endpoint::resolve_project_dir(host.as_deref())
        });
        let settings = match project.as_deref().map(DccFeedSettings::load) {
            None => DccFeedSettings::default(),
            Some(Ok(settings)) => settings,
            Some(Err(err)) => {
                tracing::warn!(target: "regia_agent", "[dcc_feed]: {err}; the feed stays off");
                DccFeedSettings::default()
            }
        };
        // Without a project (a device) the files go next to the session logs.
        let state_dir = project
            .as_deref()
            .map(net::feed_dir)
            .or_else(|| crate::default_data_dir(app).map(|logs| logs.with_file_name("dcc_feed")));
        let mut server = DccFeedServer::with_state_dir(&settings, project, state_dir);
        if settings.enabled
            && let Err(err) = server.start(&settings)
        {
            tracing::warn!(target: "regia_agent", "the DCC feed did not start: {err}");
        }
        app.insert_resource(settings).insert_resource(server);
    }
}

/// `DccFeedSet::Ingest`: advances the clock, runs the core's tick and keeps what it drained.
pub(super) fn ingest(world: &mut World) {
    let Some(server) = world.get_resource::<DccFeedServer>() else {
        return;
    };
    if !server.is_live() {
        return;
    }
    let _span = tracing::trace_span!("dcc_feed_ingest").entered();
    let core = server.core().clone();
    let replaying = lock(&core).replay.is_some();
    let real_delta = world
        .get_resource::<RealTime>()
        .map_or(0.0, |real| real.delta().as_secs_f64());
    let mut clock = world.resource_mut::<DccFeedClock>();
    let step = match clock.fixed_step_s {
        Some(step) => step,
        None if replaying => REPLAY_STEP_S,
        None => real_delta,
    };
    clock.now_s += step;
    let now = clock.now_s;
    let output = lock(&core).begin_tick(now);
    world.resource_mut::<DccFeedRegistry>().pending = output;
}
