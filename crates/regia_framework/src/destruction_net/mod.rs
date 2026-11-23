// SPDX-License-Identifier: MPL-2.0
//! Destruction level 2 over `regia_net`: the server decides, clients replay (ADR-1967, guide
//! `book/agents/destruction-net.md`). Feature `net`.
//!
//! The server's [`regia_physics::destruction::DestructionWorld`] is the only one that sees hits.
//! Each fixed tick [`destruction_net_server`] turns what changed into a stream of ops per peer:
//! instances defined and removed, every [`BreakEvent`] in order, and now and then the server's
//! `structure_hash` so a replica can tell it lost step. A joining peer first gets a snapshot, then
//! the stream from that point on. All of it travels on one reliable-ordered message type
//! ([`DestructionMessage`]), so the replica applies exactly the server's order.
//!
//! [`destruction_net_client`] replays the stream on the [`Authority::Replica`] world, in order,
//! and so reaches the same `structure_hash`; debris, sound and effect hooks follow from the
//! replayed events as they do on the server. A hash that differs, a gap or an event that does not
//! apply makes it ask for a fresh snapshot. A client can never damage anything: it sends the
//! server its input, and the game's server code calls `damage`.
//!
//! Breaks most of the time travel in a short form
//! ([`CompactBreak`](regia_physics::destruction::CompactBreak)): a bond index, or a chunk and
//! its velocity, from which the replica rebuilds the event bit for bit. The server only sends that
//! form after rebuilding the event itself, and never for a structure that moved, whose pose a
//! replica may not have yet. A burst is paced by [`DestructionNetConfig::max_ops_per_tick`], so a
//! building coming down trails on the client instead of filling the reliable queue.
//!
//! Both sides must know the assets: a game registers each [`DestructibleAsset`] it may place with
//! [`DestructionNet::register_asset`] on every process, and the stream names them by
//! [`asset_key`]. Assets a replica does not have hold its stream back ([`DestructionNetStats`]
//! lists them) until they are registered.
//!
//! Hooks: add [`DestructionNetPlugin`] after `regia_net::NetPlugin` and
//! `regia_physics::destruction::DestructionPlugin`. Everything is plain systems on the resources,
//! so a test can run them under `regia_net::testing::LoopbackHub`.

mod playback;
pub mod wire;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_ecs::ecs::system::SystemParam;
use regia_net::message::{NetMessages, NetSender, register_net_message};
use regia_net::model::{NetId, NetMode, PeerId};
use regia_net::plugin::NetSet;
use regia_net::relevance::NetInterest;
use regia_physics::destruction::{
    Authority, BreakEvent, DestructibleAsset, DestructibleId, DestructiblePose, DestructibleRoot,
    Destruction, DestructionBroke, DestructionSnapshot, fold_event_digest, follow_root_transforms,
    step_destruction,
};
use regia_physics::world::PhysicsWorld;
use regia_scene::Transform;
use serde::Serialize;

pub use playback::{DestructionPlayback, InstancePlayback, destruction_playback};
pub use wire::{
    DestructionMessage, DestructionPoses, DestructionResync, MAX_OPS_PER_MESSAGE, MAX_POSES_PER_MESSAGE,
    SnapshotPart, StreamBatch, WireInstance, WireOp,
};

/// Most bytes one snapshot message is filled to (`snapshot_part_bytes`); the protocol's limit is
/// 64 KiB.
pub const MAX_SNAPSHOT_PART_BYTES: u32 = 48 * 1024;

/// Fewest bytes `snapshot_part_bytes` may ask for.
pub const MIN_SNAPSHOT_PART_BYTES: u32 = 1024;

/// A peer that just got its snapshot catches up on the stream held back meanwhile at this many
/// times `max_ops_per_tick` per tick, so it reaches the server even while the stream runs full.
pub const CATCH_UP_FACTOR: u32 = 4;

/// Ops the server holds for one peer behind the stream. Past this, the peer's backlog is dropped
/// and it gets a fresh snapshot instead (`backlog_resyncs`).
pub const MAX_PEER_BACKLOG: u64 = 1 << 18;

/// Bytes one stream message is filled to, at most.
const STREAM_BATCH_BYTES: usize = 56 * 1024;

/// Most snapshot parts sent to one peer per tick. Their bytes stay under the transport's limit of
/// 1 MiB of reliable messages per tick, which disconnects a peer, with room for the stream.
pub const MAX_SNAPSHOT_PARTS_PER_TICK: u32 = 16;

/// Stream items a replica holds while it waits for an asset, before it gives up on them and asks
/// for a snapshot.
const MAX_PENDING_ITEMS: usize = 1 << 16;

/// The name both sides know an asset by: FNV-1a 64 of its `.destructible.ron` text, so equal
/// content has equal keys on every platform.
#[must_use]
pub fn asset_key(asset: &DestructibleAsset) -> u64 {
    let text = asset.to_ron().unwrap_or_else(|_| asset.name.clone());
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for b in text.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// How the server paces the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DestructionNetConfig {
    /// Ops sent per tick, at most [`MAX_OPS_PER_MESSAGE`]; the rest wait in order. At the default
    /// a 60 Hz stream of breaks costs at most about 220 KiB/s.
    pub max_ops_per_tick: u32,
    /// Ticks between hash checks while nothing breaks (0: only after breaks).
    pub check_every_ticks: u32,
    /// Ticks between repeats of the poses of moved structures (the channel may lose one).
    pub pose_refresh_ticks: u32,
    /// Shortest gap between two snapshots for one peer, ticks.
    pub resync_cooldown_ticks: u32,
    /// Ticks a replica waits for an asked snapshot before it asks again; each part that arrives
    /// starts the wait over.
    pub resync_retry_ticks: u32,
    /// Snapshot parts (up to `snapshot_part_bytes` each) sent to a peer per tick, 1 to
    /// [`MAX_SNAPSHOT_PARTS_PER_TICK`]. At the default a 60 Hz server sends about 2.9 MB/s.
    pub snapshot_parts_per_tick: u32,
    /// Bytes one snapshot part is filled to, [`MIN_SNAPSHOT_PART_BYTES`] to
    /// [`MAX_SNAPSHOT_PART_BYTES`] (the default). Smaller parts send a snapshot more slowly, for a
    /// slow link; an instance bigger than a part goes in pieces. It also caps the bytes of
    /// instances a peer gains through relevance per tick (times `snapshot_parts_per_tick`).
    /// A gain that does not fit waits; `held` records that instance only after its last piece
    /// is queued. One piece may exceed the cap when the tick has sent nothing yet.
    pub snapshot_part_bytes: u32,
}

impl Default for DestructionNetConfig {
    fn default() -> Self {
        Self {
            max_ops_per_tick: 256,
            check_every_ticks: 120,
            pose_refresh_ticks: 30,
            resync_cooldown_ticks: 60,
            resync_retry_ticks: 120,
            snapshot_parts_per_tick: 1,
            snapshot_part_bytes: MAX_SNAPSHOT_PART_BYTES,
        }
    }
}

impl DestructionNetConfig {
    /// `snapshot_part_bytes` within its range.
    #[must_use]
    pub fn part_bytes(&self) -> usize {
        self.snapshot_part_bytes
            .clamp(MIN_SNAPSHOT_PART_BYTES, MAX_SNAPSHOT_PART_BYTES) as usize
    }
}

/// Counters of one process (`physics.destruction_net`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DestructionNetStats {
    /// Peers the server streams to.
    pub peers: u32,
    /// Ops some peer has not had yet: the longest backlog behind the send budget (server).
    pub queued_ops: u32,
    /// Stream messages sent, summed over peers.
    pub messages_sent: u64,
    /// Breaks sent in the short form, summed over peers.
    pub compact_breaks_sent: u64,
    /// Breaks sent in full, summed over peers.
    pub full_breaks_sent: u64,
    /// Snapshots started, whole or still being sent.
    pub snapshots_sent: u64,
    /// Snapshots still being sent, one per peer at most.
    pub snapshots_in_progress: u32,
    /// Resync requests answered.
    pub resyncs_served: u64,
    /// Messages the transport refused (too large, peer gone).
    pub send_errors: u64,
    /// Breaks applied (client).
    pub breaks_applied: u64,
    /// Hash checks that matched (client).
    pub checks_passed: u64,
    /// Hash checks that differed (client).
    pub checks_failed: u64,
    /// Snapshots applied (client).
    pub snapshots_applied: u64,
    /// Snapshots this client asked for after losing step.
    pub resyncs_requested: u64,
    /// Stream items waiting to apply (client).
    pub pending_items: u32,
    /// Asset keys the client waits for.
    pub missing_assets: Vec<u64>,
    /// The agent code of the last problem (client): `destruction_bad_event`,
    /// `destruction_bad_snapshot`, `destruction_hash_mismatch`, `destruction_stream_gap` or
    /// `destruction_resync_asked`.
    pub last_problem: Option<String>,
    /// Peers whose stream relevance filters (server).
    pub relevance_peers: u32,
    /// Instances sent to a peer because they became relevant to it, summed over peers (server).
    pub instances_gained: u64,
    /// Instances taken from a peer because they stopped being relevant to it, summed over peers
    /// (server).
    pub instances_lost: u64,
    /// Breaks left out of a peer's stream because it does not hold the instance, summed over peers
    /// (server).
    pub breaks_skipped: u64,
    /// Peers catching up on the stream held back during their snapshot (server).
    pub catching_up: u32,
    /// Peers whose backlog passed [`MAX_PEER_BACKLOG`] and got a snapshot instead (server).
    pub backlog_resyncs: u64,
    /// Instances too big for one message, sent in pieces (server).
    pub split_instances: u64,
    /// Poses sent, summed over peers (server).
    pub poses_sent: u64,
    /// Poses left out for a peer because it does not hold the instance, summed over peers (server,
    /// ADR-2573).
    pub poses_filtered: u64,
    /// Poses received, whether this client holds the instance or not (client).
    pub poses_received: u64,
}

/// What the server remembers of an instance.
#[derive(Clone, Copy, Debug)]
struct Announced {
    asset: u64,
    pose: DestructiblePose,
    /// It moved since it was defined: replicas may not have its pose, so its breaks go in full.
    moved: bool,
    /// Its pose changed since the last pose message.
    dirty: bool,
}

#[derive(Clone, Debug)]
enum ServerOp {
    Wire(WireOp),
    /// A break, with the event digest after it while the server knows it.
    Break(BreakEvent, Option<u64>),
}

#[derive(Clone, Debug, Default)]
struct PeerState {
    /// The next op this peer gets.
    cursor: u64,
    sync: u32,
    last_snapshot_tick: u64,
    /// Ops went to this peer since its last hash check.
    unchecked: bool,
    last_check_tick: u64,
    /// While the cursor is short of this op, the peer catches up at [`CATCH_UP_FACTOR`].
    catch_up_until: u64,
    /// Its backlog was dropped: nothing goes to it until its next snapshot.
    stalled: bool,
    /// The instances this peer holds while relevance filters its stream; `None`: all of them.
    /// An instance is recorded here only after the last piece of its relevance gain is queued.
    held: Option<BTreeSet<DestructibleId>>,
    /// Instance whose relevance gain is still going out, piece by piece.
    gain_id: Option<DestructibleId>,
    /// Pieces of `gain_id` not sent yet.
    gain_rest: VecDeque<WireInstance>,
}

/// A snapshot on its way to one peer. Its stream waits on the server until the last part is out.
#[derive(Debug)]
struct SnapshotSend {
    parts: Arc<Vec<SnapshotPart>>,
    sync: u32,
    next: usize,
}

#[derive(Debug, Default)]
struct ServerSide {
    announced: BTreeMap<DestructibleId, Announced>,
    /// Ops some peer has not had yet, oldest first, with their numbers.
    ops: VecDeque<(u64, ServerOp)>,
    next_op: u64,
    peers: BTreeMap<PeerId, PeerState>,
    /// Peers that asked for a snapshot and wait out their cooldown.
    resync_wanted: BTreeSet<PeerId>,
    /// Snapshots under way; a new one for the same peer replaces the old.
    snapshots: BTreeMap<PeerId, SnapshotSend>,
    ticks: u64,
    /// The event sequence and digest after the last queued break, while known.
    chain: Option<(u64, u64)>,
}

#[derive(Clone, Debug)]
enum ClientItem {
    Snapshot(Vec<WireInstance>, DestructionSnapshot),
    Op { seq: Option<u64>, op: WireOp },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ClientPhase {
    /// No snapshot yet this session.
    #[default]
    Waiting,
    /// In step.
    Live,
    /// Lost step; the stream is dropped until a snapshot comes.
    Resyncing,
}

#[derive(Debug, Default)]
struct ClientSide {
    phase: ClientPhase,
    assembling: Option<(u32, u16, DestructionSnapshot, Vec<WireInstance>)>,
    /// The pieces so far of an instance that gained relevance.
    piece: Option<WireInstance>,
    pending: VecDeque<ClientItem>,
    last_request_tick: u64,
    ticks: u64,
    need_resync: bool,
}

/// The state of destruction over the network, on every process.
#[derive(Resource, Debug, Default)]
pub struct DestructionNet {
    /// Pacing of the stream.
    pub config: DestructionNetConfig,
    stats: DestructionNetStats,
    assets: BTreeMap<u64, Arc<DestructibleAsset>>,
    /// Keys by `Arc` address, for the `Arc`s in `assets`, which keep those addresses alive.
    key_of: HashMap<usize, u64>,
    server: ServerSide,
    client: ClientSide,
    /// The replicated entity whose relevance decides who gets each instance (ADR-2305).
    anchors: BTreeMap<DestructibleId, Entity>,
}

impl DestructionNet {
    /// Makes an asset known by its [`asset_key`]; do this on every process for every asset the
    /// server may place (the server learns the ones it places by itself).
    pub fn register_asset(&mut self, asset: Arc<DestructibleAsset>) -> u64 {
        let address = Arc::as_ptr(&asset) as usize;
        if let Some(key) = self.key_of.get(&address) {
            return *key;
        }
        let key = asset_key(&asset);
        // An equal asset in another `Arc` is not kept: its address could come back as something
        // else once it drops, so it is hashed again next time.
        self.assets.entry(key).or_insert_with(|| {
            self.key_of.insert(address, key);
            asset
        });
        key
    }

    /// Counters of this process.
    #[must_use]
    pub fn stats(&self) -> &DestructionNetStats {
        &self.stats
    }

    /// Whether this client has applied everything it got and lost no step (always `true` on a
    /// server). It may still be behind the server by the stream's latency and pacing.
    #[must_use]
    pub fn in_step(&self, mode: NetMode) -> bool {
        !mode.is_client()
            || (self.client.phase == ClientPhase::Live
                && self.client.assembling.is_none()
                && self.client.pending.is_empty())
    }

    /// Text name of this client's phase, for tools.
    #[must_use]
    pub fn client_phase(&self) -> &'static str {
        match self.client.phase {
            ClientPhase::Waiting => "waiting",
            ClientPhase::Live => "live",
            ClientPhase::Resyncing => "resyncing",
        }
    }

    /// Asks the server for a fresh snapshot on the next tick, as a client that lost step does.
    pub fn request_resync(&mut self) {
        lose_step(self, "destruction_resync_asked");
    }

    /// Drops a client's in-flight state so the next snapshot starts it afresh: call it when a
    /// session ends (the world stays as it was).
    pub fn reset_session(&mut self) {
        self.client = ClientSide::default();
        self.server = ServerSide::default();
    }

    /// Streams instance `id` only to the peers that hold `anchor` (server, ADR-2305): a replicated
    /// entity (`regia_net::Replicated`, with a `NetId`) that the spec 72 relevance pipeline
    /// (`regia_net::relevance::NetInterest`) decides for, such as the building the instance belongs
    /// to or its root entity made replicated. Its profile, visibility lists, groups, custom
    /// filters and grants all apply. A peer that does not hold the anchor does not hold the
    /// instance: its breaks skip that peer, and it gets the instance's state when it gains the
    /// anchor. Instances without an anchor go to every peer. Without a `NetInterest` resource
    /// every instance goes to every peer.
    pub fn set_relevance_anchor(&mut self, id: DestructibleId, anchor: Entity) {
        self.anchors.insert(id, anchor);
    }

    /// Streams instance `id` to every peer again.
    pub fn clear_relevance_anchor(&mut self, id: DestructibleId) -> Option<Entity> {
        self.anchors.remove(&id)
    }

    /// The anchor of an instance ([`Self::set_relevance_anchor`]).
    #[must_use]
    pub fn relevance_anchor(&self, id: DestructibleId) -> Option<Entity> {
        self.anchors.get(&id).copied()
    }

    /// Every instance with a relevance anchor, in id order.
    pub fn relevance_anchors(&self) -> impl Iterator<Item = (DestructibleId, Entity)> + '_ {
        self.anchors.iter().map(|(id, e)| (*id, *e))
    }

    /// The peers relevance filters (server), in peer order, with the instances each holds.
    #[must_use]
    pub fn filtered_peers(&self) -> Vec<(PeerId, Vec<DestructibleId>)> {
        self.server
            .peers
            .iter()
            .filter_map(|(p, s)| Some((*p, s.held.as_ref()?.iter().copied().collect())))
            .collect()
    }

    /// Instances held by `peer` while relevance filters its stream (server); `None` while it gets
    /// every instance or is not a peer.
    #[must_use]
    pub fn held_by(&self, peer: PeerId) -> Option<Vec<DestructibleId>> {
        let held = self.server.peers.get(&peer)?.held.as_ref()?;
        Some(held.iter().copied().collect())
    }
}

/// Who gets which instance this tick (ADR-2305).
struct Relevance<'a> {
    /// The `NetId` of each anchor; `None` while the anchor has none yet.
    anchors: BTreeMap<DestructibleId, Option<NetId>>,
    interest: Option<&'a NetInterest>,
}

impl Relevance<'_> {
    /// Some instance has an anchor and the relevance pipeline runs.
    fn active(&self) -> bool {
        self.interest.is_some() && !self.anchors.is_empty()
    }

    fn relevant(&self, peer: PeerId, id: DestructibleId) -> bool {
        let Some(interest) = self.interest else {
            return true;
        };
        match self.anchors.get(&id) {
            None => true,
            Some(None) => false,
            Some(Some(net_id)) => interest.holds(peer, *net_id),
        }
    }
}

/// Registers the messages and the [`DestructionNet`] resource on a world, in the same order on
/// every process (what [`DestructionNetPlugin`] does).
pub fn install_destruction_net(world: &mut World) {
    register_net_message::<DestructionMessage>(world);
    register_net_message::<DestructionResync>(world);
    register_net_message::<DestructionPoses>(world);
    world.init_resource::<DestructionNet>();
}

/// Destruction over the network: add it after `regia_net::NetPlugin` and `DestructionPlugin`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DestructionNetPlugin;

impl Plugin for DestructionNetPlugin {
    fn build(&self, app: &mut App) {
        install_destruction_net(app.world_mut());
        app.add_systems(
            FixedUpdate,
            destruction_net_client
                .after(NetSet::Apply)
                .before(follow_root_transforms),
        )
        .add_systems(
            FixedUpdate,
            destruction_net_server
                .after(step_destruction)
                .before(NetSet::Assign),
        );
    }
}

/// The mode of this process.
fn mode_of(mode: Option<&NetMode>) -> NetMode {
    mode.copied().unwrap_or_default()
}

/// What the server reads to decide who holds which instance (ADR-2305).
#[derive(SystemParam)]
pub struct RelevanceInputs<'w, 's> {
    interest: Option<Res<'w, NetInterest>>,
    net_ids: Query<'w, 's, &'static NetId>,
}

/// Server: sends this tick's changes (after `step_destruction`). Does nothing but read the break
/// messages on a client.
pub fn destruction_net_server(
    mut net: ResMut<DestructionNet>,
    mut destruction: ResMut<Destruction>,
    mut broke: MessageReader<DestructionBroke>,
    resyncs: Option<Res<NetMessages<DestructionResync>>>,
    mode: Option<Res<NetMode>>,
    inputs: RelevanceInputs,
    mut sender: NetSender,
) {
    let mode = mode_of(mode.as_deref());
    let events: Vec<BreakEvent> = broke.read().map(|m| m.0.clone()).collect();
    if !mode.is_server() {
        return;
    }
    let _span = tracing::debug_span!(target: "regia_framework", "destruction_net_server").entered();
    let net = &mut *net;
    if destruction.world.authority() == Authority::Replica {
        // A session ended: this world decides again, and every peer starts from a snapshot.
        destruction.world.set_authority(Authority::Server);
        net.client = ClientSide::default();
    }
    if mode == NetMode::Offline {
        // Nobody to tell, and nothing to remember: a session that starts later begins with a
        // snapshot of whatever this world holds by then.
        if !net.server.announced.is_empty() || !net.server.peers.is_empty() {
            net.server = ServerSide::default();
        }
        return;
    }
    net.server.ticks += 1;
    let peers = sender.peers();
    net.server.peers.retain(|p, _| peers.contains(p));
    net.server.resync_wanted.retain(|p| peers.contains(p));
    net.server.snapshots.retain(|p, _| peers.contains(p));
    let world = &destruction.world;

    // What changed: new instances first (an event can name them), removals last (an event can
    // name them too).
    let mut ops: Vec<ServerOp> = Vec::new();
    for (id, inst) in world.instances() {
        let pose = inst.pose();
        if let Some(a) = net.server.announced.get_mut(&id) {
            if a.pose != pose {
                a.pose = pose;
                a.moved = true;
                a.dirty = true;
            }
            continue;
        }
        let asset = net.register_asset(inst.asset().clone());
        net.server.announced.insert(
            id,
            Announced {
                asset,
                pose,
                moved: false,
                dirty: false,
            },
        );
        ops.push(ServerOp::Wire(WireOp::Define {
            id,
            asset,
            pose,
            seed: inst.seed(),
        }));
    }
    // The digest after each break, for peers that skip it (ADR-2305): folded on from the last
    // known point, and given up (`None`) for a tick whose fold does not land on the world's.
    let first_break = ops.len();
    for event in events {
        let after = match net.server.chain {
            Some((seq, digest)) if seq == event.seq => Some(fold_event_digest(digest, &event)),
            _ => None,
        };
        net.server.chain = after.map(|d| (event.seq + 1, d));
        ops.push(ServerOp::Break(event, after));
    }
    let now = (world.event_count(), world.event_digest());
    if net.server.chain != Some(now) {
        for op in &mut ops[first_break..] {
            if let ServerOp::Break(_, digest) = op {
                *digest = None;
            }
        }
        net.server.chain = Some(now);
    }
    let gone: Vec<DestructibleId> = net
        .server
        .announced
        .keys()
        .copied()
        .filter(|id| world.instance(*id).is_none())
        .collect();
    for id in gone {
        net.server.announced.remove(&id);
        net.anchors.remove(&id);
        ops.push(ServerOp::Wire(WireOp::Remove { id }));
    }
    for op in ops {
        let no = net.server.next_op;
        net.server.next_op += 1;
        net.server.ops.push_back((no, op));
    }

    let relevance = Relevance {
        anchors: net
            .anchors
            .iter()
            .map(|(id, e)| (*id, inputs.net_ids.get(*e).ok().copied()))
            .collect(),
        interest: inputs.interest.as_deref(),
    };

    // New peers and peers that lost step get a snapshot: it holds every op queued so far.
    let tick = net.server.ticks;
    let cooldown = u64::from(net.config.resync_cooldown_ticks);
    let mut want: BTreeSet<PeerId> = peers
        .iter()
        .copied()
        .filter(|p| !net.server.peers.contains_key(p))
        .collect();
    if let Some(requests) = resyncs.as_deref() {
        for r in requests.iter() {
            tracing::info!(target: "regia_framework", peer = ?r.from, have_seq = r.msg.have_seq, "destruction resync asked");
            net.server.resync_wanted.insert(r.from);
        }
    }
    let due: Vec<PeerId> = net
        .server
        .resync_wanted
        .iter()
        .copied()
        .filter(|p| {
            net.server
                .peers
                .get(p)
                .is_some_and(|s| tick.saturating_sub(s.last_snapshot_tick) >= cooldown)
        })
        .collect();
    for peer in due {
        net.server.resync_wanted.remove(&peer);
        if want.insert(peer) {
            net.stats.resyncs_served += 1;
        }
    }
    if !want.is_empty() {
        let full = destruction.world.snapshot();
        let mut shared: Option<Arc<Vec<SnapshotPart>>> = None;
        for peer in want {
            let (parts, held) = if relevance.active() {
                let held: BTreeSet<DestructibleId> = net
                    .server
                    .announced
                    .keys()
                    .copied()
                    .filter(|id| relevance.relevant(peer, *id))
                    .collect();
                (Arc::new(snapshot_parts(net, &full, Some(&held))), Some(held))
            } else {
                let parts = match &shared {
                    Some(parts) => parts.clone(),
                    None => {
                        let parts = Arc::new(snapshot_parts(net, &full, None));
                        shared = Some(parts.clone());
                        parts
                    }
                };
                (parts, None)
            };
            let next_op = net.server.next_op;
            let state = net.server.peers.entry(peer).or_default();
            state.sync = state.sync.wrapping_add(1);
            state.cursor = next_op;
            state.catch_up_until = next_op;
            state.last_snapshot_tick = tick;
            state.stalled = false;
            state.held = held;
            // The snapshot carries every instance this peer holds; a half-sent gain is superseded
            // and its leftover pieces must not follow it (corr-framework-a F2).
            state.gain_id = None;
            state.gain_rest.clear();
            let send = SnapshotSend {
                parts,
                sync: state.sync,
                next: 0,
            };
            net.server.snapshots.insert(peer, send);
            net.stats.snapshots_sent += 1;
        }
    }
    send_snapshots(net, &mut sender);

    if peers.is_empty() {
        net.server.ops.clear();
    } else {
        send_stream(net, &destruction, &relevance, &mut sender);
    }
    send_poses(net, &mut sender);
    net.stats.peers = peers.len() as u32;
    net.stats.queued_ops = net.server.ops.len() as u32;
    net.stats.snapshots_in_progress = net.server.snapshots.len() as u32;
    net.stats.relevance_peers = net.server.peers.values().filter(|s| s.held.is_some()).count() as u32;
    net.stats.catching_up = net
        .server
        .peers
        .values()
        .filter(|s| s.cursor < s.catch_up_until)
        .count() as u32;
}

/// Sends the next parts of every snapshot under way. The first part goes out on the tick the
/// snapshot was taken; the peer's stream waits on the server until the last part is out, then
/// catches up. A part the transport refuses restarts that peer's snapshot.
fn send_snapshots(net: &mut DestructionNet, sender: &mut NetSender) {
    let budget = net
        .config
        .snapshot_parts_per_tick
        .clamp(1, MAX_SNAPSHOT_PARTS_PER_TICK) as usize;
    let mut refused = Vec::new();
    for (peer, send) in &mut net.server.snapshots {
        let end = (send.next + budget).min(send.parts.len());
        for part in &send.parts[send.next..end] {
            let mut part = part.clone();
            part.sync = send.sync;
            if sender
                .send_session_to_peer(*peer, DestructionMessage::Snapshot(part))
                .is_err()
            {
                net.stats.send_errors += 1;
                refused.push(*peer);
                break;
            }
        }
        send.next = end;
    }
    let next_op = net.server.next_op;
    let done: Vec<PeerId> = net
        .server
        .snapshots
        .iter()
        .filter(|(_, send)| send.next >= send.parts.len())
        .map(|(p, _)| *p)
        .collect();
    for peer in done {
        net.server.snapshots.remove(&peer);
        if let Some(state) = net.server.peers.get_mut(&peer) {
            // What queued during the send goes out faster until the peer is level again.
            state.catch_up_until = next_op;
        }
    }
    for peer in refused {
        net.server.snapshots.remove(&peer);
        net.server.resync_wanted.insert(peer);
    }
}

/// The world as messages (`sync` is filled per peer): only the instances in `keep` when it is
/// given. An instance bigger than a part goes in pieces, which the client joins.
fn snapshot_parts(
    net: &mut DestructionNet,
    snapshot: &DestructionSnapshot,
    keep: Option<&BTreeSet<DestructibleId>>,
) -> Vec<SnapshotPart> {
    let part_bytes = net.config.part_bytes();
    let header = DestructionSnapshot {
        next_id: snapshot.next_id,
        next_seq: snapshot.next_seq,
        event_digest: snapshot.event_digest,
        tick: snapshot.tick,
        instances: Vec::new(),
    };
    let mut parts: Vec<SnapshotPart> = vec![SnapshotPart {
        header,
        ..SnapshotPart::default()
    }];
    let mut bytes = 0usize;
    for state in &snapshot.instances {
        if keep.is_some_and(|k| !k.contains(&state.id)) {
            continue;
        }
        let Some(asset) = net.server.announced.get(&state.id).map(|a| a.asset) else {
            continue;
        };
        let whole = WireInstance {
            asset,
            state: state.clone(),
        };
        let pieces = wire::split_instance(&whole, part_bytes);
        if pieces.len() > 1 {
            net.stats.split_instances += 1;
        }
        for piece in pieces {
            let size = wire::instance_bytes(&piece);
            if bytes > 0 && bytes + size > part_bytes {
                parts.push(SnapshotPart::default());
                bytes = 0;
            }
            bytes += size;
            if let Some(last) = parts.last_mut() {
                last.instances.push(piece);
            }
        }
    }
    let total = parts.len().min(usize::from(u16::MAX)) as u16;
    for (i, part) in parts.iter_mut().enumerate() {
        part.index = i.min(usize::from(total).saturating_sub(1)) as u16;
        part.total = total;
    }
    parts
}

/// One peer's stream messages of this tick, filled op by op.
struct Outgoing {
    batches: Vec<StreamBatch>,
    batch: StreamBatch,
    bytes: usize,
    /// The message being filled holds a break.
    has_break: bool,
    /// Breaks were left out since the last op: the sequence and digest after them.
    skip: Option<(u64, Option<u64>)>,
}

impl Outgoing {
    fn new() -> Self {
        Self {
            batches: Vec::new(),
            batch: StreamBatch::default(),
            bytes: 0,
            has_break: false,
            skip: None,
        }
    }

    fn push(&mut self, op: WireOp, bytes: usize) {
        if !self.batch.ops.is_empty()
            && (self.batch.ops.len() >= MAX_OPS_PER_MESSAGE || self.bytes + bytes > STREAM_BATCH_BYTES)
        {
            self.batches.push(std::mem::take(&mut self.batch));
            self.bytes = 0;
            self.has_break = false;
        }
        self.bytes += bytes;
        self.batch.ops.push(op);
    }

    /// A break: the first of a message sets where its sequence and ticks count from.
    fn push_break(&mut self, event: &BreakEvent, op: WireOp) {
        self.push(op, 64);
        if !self.has_break {
            self.has_break = true;
            self.batch.first_seq = event.seq;
            self.batch.base_tick = event.tick;
        }
    }

    /// Writes the skip that is waiting; `false` when its digest is unknown.
    fn flush_skip(&mut self) -> bool {
        match self.skip.take() {
            None => true,
            Some((next_seq, Some(digest))) => {
                self.push(WireOp::Skip { next_seq, digest }, 20);
                true
            }
            Some((_, None)) => false,
        }
    }

    fn finish(mut self) -> Vec<StreamBatch> {
        if !self.batch.ops.is_empty() {
            self.batches.push(self.batch);
        }
        self.batches
    }
}

/// Queues one in-progress relevance gain until `gain_bytes` is used. The first piece of a tick
/// is sent even when it is larger than the budget (a part is about `snapshot_part_bytes`, and
/// the queued op adds two bytes).
fn queue_gain_pieces(
    state: &mut PeerState,
    gain_bytes: usize,
    mut bytes: usize,
    out: &mut Outgoing,
) -> usize {
    while let Some(piece) = state.gain_rest.pop_front() {
        let size = wire::instance_bytes(&piece) + 2;
        // A later piece waits for the next tick. The first piece of a tick still goes out.
        if bytes > 0 && bytes.saturating_add(size) > gain_bytes {
            state.gain_rest.push_front(piece);
            break;
        }
        bytes = bytes.saturating_add(size);
        let more = !state.gain_rest.is_empty();
        out.push(
            WireOp::Instance {
                instance: piece,
                more,
            },
            size,
        );
        if bytes >= gain_bytes {
            break;
        }
    }
    bytes
}

/// Records `gain_id` as held once every piece has been queued.
fn finish_gain(state: &mut PeerState, stats: &mut DestructionNetStats) {
    if let Some(id) = state.gain_id.take() {
        if let Some(held) = state.held.as_mut() {
            held.insert(id);
        }
        stats.instances_gained += 1;
        state.unchecked = true;
    }
}

/// The announced set, the live world, and the byte budget one relevance-gain tick reads.
struct GainSend<'a> {
    announced: &'a BTreeMap<DestructibleId, Announced>,
    world: &'a regia_physics::destruction::DestructionWorld,
    part_bytes: usize,
    gain_bytes: usize,
}

/// Sends instances that became relevant, at most `gain_bytes` this tick. Pieces of one instance
/// stay queued on the peer so a later tick continues that instance instead of splitting a newer
/// snapshot. `held` records the instance only after the last piece is queued.
///
/// A gain that has started always finishes (corr-framework-a F2): the client already holds its
/// first pieces, and a different instance's piece would not join them. While it is under way the
/// peer's op stream waits (`send_stream`), so the pieces, cut at the stream position where the
/// gain began, stay consistent with it; breaks of the instance in the meantime go out after the
/// last piece, once the instance is held. An instance that stopped being relevant or was removed
/// meanwhile leaves the peer through the usual `Remove` once it is held. A new gain starts only
/// when `caught_up` (the peer's cursor is at the newest op): its pieces are cut from the world
/// now, so ops still pending for the peer would otherwise apply to that instance twice.
fn send_relevance_gains(
    state: &mut PeerState,
    peer: PeerId,
    relevance: &Relevance<'_>,
    send: &GainSend<'_>,
    caught_up: bool,
    stats: &mut DestructionNetStats,
    out: &mut Outgoing,
) {
    if state.held.is_none() {
        return;
    }
    let mut bytes = 0usize;
    if state.gain_id.is_some() {
        bytes = queue_gain_pieces(state, send.gain_bytes, bytes, out);
        if state.gain_rest.is_empty() {
            finish_gain(state, stats);
        } else {
            return;
        }
    }
    if !caught_up {
        return;
    }
    let ids: Vec<DestructibleId> = send.announced.keys().copied().collect();
    for id in ids {
        if bytes >= send.gain_bytes {
            break;
        }
        if state.held.as_ref().is_some_and(|held| held.contains(&id)) || !relevance.relevant(peer, id) {
            continue;
        }
        let Some(row) = send.announced.get(&id) else {
            continue;
        };
        let Some(snap) = send.world.instance_snapshot(id) else {
            continue;
        };
        let pieces = wire::split_instance(
            &WireInstance {
                asset: row.asset,
                state: snap,
            },
            send.part_bytes,
        );
        if pieces.len() > 1 {
            stats.split_instances += 1;
        }
        state.gain_id = Some(id);
        state.gain_rest = VecDeque::from(pieces);
        bytes = queue_gain_pieces(state, send.gain_bytes, bytes, out);
        if state.gain_rest.is_empty() {
            finish_gain(state, stats);
        } else {
            break;
        }
    }
    // Back to the whole world once nothing filters and the peer holds it all (every announced
    // instance, not just as many: a finished gain of a removed instance stays held until its
    // `Remove` goes out).
    if !relevance.active()
        && state.gain_id.is_none()
        && state
            .held
            .as_ref()
            .is_some_and(|held| send.announced.keys().all(|id| held.contains(id)))
    {
        state.held = None;
    }
}

/// Sends each peer the next ops from its cursor, up to the budget, then a hash check once it has
/// them all. A peer that relevance filters gets only the instances it holds: what it lost, what
/// it gained, and skips past the breaks of the rest (ADR-2305).
fn send_stream(
    net: &mut DestructionNet,
    destruction: &Destruction,
    relevance: &Relevance,
    sender: &mut NetSender,
) {
    let world = &destruction.world;
    let budget = u64::from(net.config.max_ops_per_tick.max(1)).min(MAX_OPS_PER_MESSAGE as u64 - 1);
    let part_bytes = net.config.part_bytes();
    let gain_bytes = part_bytes
        * net
            .config
            .snapshot_parts_per_tick
            .clamp(1, MAX_SNAPSHOT_PARTS_PER_TICK) as usize;
    let next_op = net.server.next_op;
    let front = net.server.ops.front().map_or(next_op, |(no, _)| *no);
    let mut world_hash: Option<u64> = None;
    let peers: Vec<PeerId> = net.server.peers.keys().copied().collect();
    for peer in peers {
        if net.server.snapshots.contains_key(&peer) {
            continue;
        }
        let Some(mut state) = net.server.peers.remove(&peer) else {
            continue;
        };
        if state.stalled {
            net.server.peers.insert(peer, state);
            continue;
        }
        if next_op.saturating_sub(state.cursor) > MAX_PEER_BACKLOG {
            tracing::warn!(target: "regia_framework", ?peer, "destruction backlog too long; sending a snapshot");
            state.cursor = next_op;
            state.stalled = true;
            net.stats.backlog_resyncs += 1;
            net.server.resync_wanted.insert(peer);
            net.server.peers.insert(peer, state);
            continue;
        }
        // Relevance turns on for a peer at a point where it holds every announced instance.
        if relevance.active() && state.held.is_none() && state.cursor == next_op {
            state.held = Some(net.server.announced.keys().copied().collect());
        }
        let allowance = if state.cursor < state.catch_up_until {
            budget * u64::from(CATCH_UP_FACTOR)
        } else {
            budget
        };
        let from = state.cursor.saturating_sub(front) as usize;
        // A relevance gain under way holds the stream where the gain began: its pieces describe
        // the instance at that point (see `send_relevance_gains`).
        let gaining = state.gain_id.is_some();
        let end = if gaining {
            from
        } else {
            (from as u64 + allowance).min(net.server.ops.len() as u64) as usize
        };
        let mut out = Outgoing::new();
        if let Some(held) = state.held.as_mut() {
            let lost: Vec<DestructibleId> = held
                .iter()
                .copied()
                .filter(|id| !relevance.relevant(peer, *id))
                .collect();
            for id in lost {
                held.remove(&id);
                net.stats.instances_lost += 1;
                out.push(WireOp::Remove { id }, 6);
            }
        }
        let mut lost_digest = false;
        for (_, op) in net.server.ops.range(from..end) {
            match op {
                ServerOp::Wire(op) => {
                    if let Some(held) = state.held.as_mut() {
                        match op {
                            WireOp::Define { id, .. } => {
                                if !relevance.relevant(peer, *id) {
                                    continue;
                                }
                                held.insert(*id);
                            }
                            WireOp::Remove { id } => {
                                // Only what this peer holds leaves it.
                                let held_it = held.remove(id);
                                if !held_it {
                                    continue;
                                }
                            }
                            _ => {}
                        }
                    }
                    if !out.flush_skip() {
                        lost_digest = true;
                        break;
                    }
                    out.push(op.clone(), 64);
                }
                ServerOp::Break(event, digest) => {
                    if state
                        .held
                        .as_ref()
                        .is_some_and(|h| !h.contains(&event.destructible))
                    {
                        out.skip = Some((event.seq + 1, *digest));
                        net.stats.breaks_skipped += 1;
                        continue;
                    }
                    if !out.flush_skip() {
                        lost_digest = true;
                        break;
                    }
                    let wire = wire_break(&net.server.announced, &mut net.stats, world, event);
                    out.push_break(event, wire);
                }
            }
        }
        if !lost_digest && !out.flush_skip() {
            lost_digest = true;
        }
        if lost_digest {
            // The server cannot tell this peer where the sequence went: a snapshot can.
            state.stalled = true;
            net.server.resync_wanted.insert(peer);
            net.server.peers.insert(peer, state);
            continue;
        }
        if end > from {
            state.unchecked = true;
        }
        state.cursor = front + end as u64;
        let caught_up = state.cursor == next_op;
        if caught_up || gaining {
            send_relevance_gains(
                &mut state,
                peer,
                relevance,
                &GainSend {
                    announced: &net.server.announced,
                    world,
                    part_bytes,
                    gain_bytes,
                },
                caught_up,
                &mut net.stats,
                &mut out,
            );
        }
        if state.cursor == next_op && state.gain_id.is_none() {
            let periodic = net.config.check_every_ticks > 0
                && net.server.ticks.saturating_sub(state.last_check_tick)
                    >= u64::from(net.config.check_every_ticks);
            if state.unchecked || periodic {
                let hash = match &state.held {
                    None => *world_hash.get_or_insert_with(|| world.structure_hash()),
                    Some(held) => world.structure_hash_where(|id| held.contains(&id)),
                };
                out.push(WireOp::Check { hash }, 9);
                state.unchecked = false;
                state.last_check_tick = net.server.ticks;
            }
        }
        for batch in out.finish() {
            net.stats.messages_sent += 1;
            if sender
                .send_session_to_peer(peer, DestructionMessage::Stream(batch))
                .is_err()
            {
                net.stats.send_errors += 1;
            }
        }
        net.server.peers.insert(peer, state);
    }
    // Ops every peer has had are done with.
    let oldest = net
        .server
        .peers
        .values()
        .map(|s| s.cursor)
        .min()
        .unwrap_or(next_op);
    while net.server.ops.front().is_some_and(|(no, _)| *no < oldest) {
        net.server.ops.pop_front();
    }
}

/// A break on the wire: the short form when the replica can rebuild it, in full otherwise.
fn wire_break(
    announced: &BTreeMap<DestructibleId, Announced>,
    stats: &mut DestructionNetStats,
    world: &regia_physics::destruction::DestructionWorld,
    event: &BreakEvent,
) -> WireOp {
    let moved = announced.get(&event.destructible).is_none_or(|a| a.moved);
    match (!moved).then(|| world.compact_event(event)).flatten() {
        Some(kind) => {
            stats.compact_breaks_sent += 1;
            WireOp::Compact {
                id: event.destructible,
                tick: event.tick,
                kind,
            }
        }
        None => {
            stats.full_breaks_sent += 1;
            WireOp::Full {
                id: event.destructible,
                tick: event.tick,
                kind: event.kind.clone(),
                seed: event.seed,
            }
        }
    }
}

/// Poses of structures that moved: every change, and every `pose_refresh_ticks` for the moved ones
/// (an unreliable message may be the last one a structure ever gets). Each peer gets the poses of
/// what it holds (ADR-2573); a structure it gains later arrives in its current pose.
fn send_poses(net: &mut DestructionNet, sender: &mut NetSender) {
    if net.server.peers.is_empty() {
        return;
    }
    let refresh = net.config.pose_refresh_ticks > 0
        && net
            .server
            .ticks
            .is_multiple_of(u64::from(net.config.pose_refresh_ticks));
    let mut poses: Vec<(DestructibleId, DestructiblePose)> = Vec::new();
    for (id, a) in &mut net.server.announced {
        if a.dirty || (a.moved && refresh) {
            a.dirty = false;
            poses.push((*id, a.pose));
        }
    }
    if poses.is_empty() {
        return;
    }
    for (peer, state) in &net.server.peers {
        let held: Vec<(DestructibleId, DestructiblePose)> = match &state.held {
            None => poses.clone(),
            Some(held) => poses
                .iter()
                .copied()
                .filter(|(id, _)| held.contains(id))
                .collect(),
        };
        net.stats.poses_filtered += (poses.len() - held.len()) as u64;
        net.stats.poses_sent += held.len() as u64;
        for chunk in held.chunks(MAX_POSES_PER_MESSAGE) {
            let msg = DestructionPoses {
                poses: chunk.to_vec(),
            };
            if sender.send_session_to_peer(*peer, msg).is_err() {
                net.stats.send_errors += 1;
            }
        }
    }
}

/// The messages a client reads this tick.
#[derive(SystemParam)]
pub struct ClientInbox<'w> {
    stream: Option<Res<'w, NetMessages<DestructionMessage>>>,
    poses: Option<Res<'w, NetMessages<DestructionPoses>>>,
}

/// Client: takes this tick's messages and replays them on the replica (before
/// `step_destruction`, whose report then carries the replayed events and effects).
pub fn destruction_net_client(
    mut net: ResMut<DestructionNet>,
    mut destruction: ResMut<Destruction>,
    mut physics: Option<ResMut<PhysicsWorld>>,
    inbox: ClientInbox,
    mode: Option<Res<NetMode>>,
    mut sender: NetSender,
    mut roots: Query<(&DestructibleRoot, &mut Transform)>,
) {
    if !mode_of(mode.as_deref()).is_client() {
        return;
    }
    let _span = tracing::debug_span!(target: "regia_framework", "destruction_net_client").entered();
    let net = &mut *net;
    if destruction.world.authority() != Authority::Replica {
        // Joined a session: the first snapshot replaces whatever this world held.
        destruction.world.set_authority(Authority::Replica);
        net.server = ServerSide::default();
    }
    let d = &mut *destruction;
    net.client.ticks += 1;
    if let Some(incoming) = inbox.stream.as_deref() {
        for received in incoming.iter() {
            receive(net, &received.msg);
        }
    }
    apply_pending(net, d, physics.as_deref_mut());
    if let Some(poses) = inbox.poses.as_deref() {
        for received in poses.iter() {
            net.stats.poses_received += received.msg.poses.len() as u64;
            for (id, pose) in &received.msg.poses {
                apply_pose(d, physics.as_deref_mut(), *id, *pose, &mut roots);
            }
        }
    }
    let retry = u64::from(net.config.resync_retry_ticks);
    let waited = net.client.ticks.saturating_sub(net.client.last_request_tick);
    let overdue = net.client.phase == ClientPhase::Resyncing && waited >= retry;
    if net.client.need_resync || overdue {
        net.client.need_resync = false;
        net.client.last_request_tick = net.client.ticks;
        net.stats.resyncs_requested += 1;
        let have_seq = d.world.event_count();
        if sender
            .send_session_to_server(DestructionResync { have_seq })
            .is_err()
        {
            net.stats.send_errors += 1;
        }
    }
    net.stats.pending_items = net.client.pending.len() as u32;
}

/// Moves a structure on a replica, with its body and root entity.
fn apply_pose(
    d: &mut Destruction,
    physics: Option<&mut PhysicsWorld>,
    id: DestructibleId,
    pose: DestructiblePose,
    roots: &mut Query<(&DestructibleRoot, &mut Transform)>,
) {
    if d.world.set_pose(id, pose).is_err() {
        return;
    }
    if let Some(physics) = physics {
        // The id comes from the world; a body that is not built yet starts at the new pose.
        let _ = d.physics.follow_pose(physics, &d.world, id);
    }
    let Some(entity) = d.root_entity(id) else {
        return;
    };
    if let Ok((_, mut transform)) = roots.get_mut(entity) {
        transform.translation = regia_core::math::Vec3::from_array(pose.translation);
        transform.rotation = regia_core::math::Quat::from_array(pose.rotation);
    }
}

/// Takes one message apart: stream ops become items, snapshot parts assemble.
fn receive(net: &mut DestructionNet, message: &DestructionMessage) {
    let client = &mut net.client;
    match message {
        DestructionMessage::Stream(batch) => {
            // Before a snapshot the stream has no base to apply to. Once its first part is in,
            // the stream waits behind it.
            if client.phase != ClientPhase::Live && client.assembling.is_none() {
                return;
            }
            let mut seq = batch.first_seq;
            for op in &batch.ops {
                let op = match op {
                    WireOp::Instance { instance, more } => {
                        // Pieces of one instance join before it applies.
                        let whole = match client.piece.take() {
                            None => instance.clone(),
                            Some(mut so_far) => {
                                if !wire::join_piece(&mut so_far, instance) {
                                    lose_step(net, "destruction_bad_snapshot");
                                    return;
                                }
                                so_far
                            }
                        };
                        if *more {
                            client.piece = Some(whole);
                            continue;
                        }
                        WireOp::Instance {
                            instance: whole,
                            more: false,
                        }
                    }
                    WireOp::Skip { next_seq, .. } => {
                        seq = *next_seq;
                        op.clone()
                    }
                    op => op.clone(),
                };
                let breaks = matches!(op, WireOp::Compact { .. } | WireOp::Full { .. });
                client.pending.push_back(ClientItem::Op {
                    seq: breaks.then_some(seq),
                    op,
                });
                seq += u64::from(breaks);
            }
        }
        DestructionMessage::Snapshot(part) => {
            if part.index == 0 {
                client.assembling = Some((part.sync, 0, part.header.clone(), Vec::new()));
                // A snapshot replaces everything before it.
                client.pending.clear();
                client.piece = None;
            }
            let Some((sync, seen, header, instances)) = client.assembling.as_mut() else {
                return;
            };
            if *sync != part.sync || *seen != part.index {
                lose_step(net, "destruction_bad_snapshot");
                return;
            }
            *seen += 1;
            client.last_request_tick = client.ticks;
            for instance in &part.instances {
                // A big instance comes in pieces with the same id; they join here.
                match instances.last_mut() {
                    Some(last) if last.state.id == instance.state.id => {
                        if !wire::join_piece(last, instance) {
                            lose_step(net, "destruction_bad_snapshot");
                            return;
                        }
                    }
                    _ => instances.push(instance.clone()),
                }
            }
            if *seen == part.total {
                let header = header.clone();
                let instances = std::mem::take(instances);
                client.assembling = None;
                client.phase = ClientPhase::Live;
                client.pending.push_front(ClientItem::Snapshot(instances, header));
            }
        }
    }
    if client.pending.len() > MAX_PENDING_ITEMS {
        client.pending.clear();
        lose_step(net, "destruction_stream_gap");
    }
}

/// This client lost step: forget the stream and ask for a snapshot.
fn lose_step(net: &mut DestructionNet, code: &str) {
    net.client.pending.clear();
    net.client.assembling = None;
    net.client.piece = None;
    net.client.phase = ClientPhase::Resyncing;
    net.client.need_resync = true;
    net.stats.last_problem = Some(code.to_string());
    tracing::warn!(target: "regia_framework", code, "destruction replica lost step; asking for a snapshot");
}

/// Applies items in order until one is blocked on an asset.
fn apply_pending(net: &mut DestructionNet, d: &mut Destruction, mut physics: Option<&mut PhysicsWorld>) {
    net.stats.missing_assets.clear();
    if net.client.assembling.is_some() {
        return;
    }
    while let Some(item) = net.client.pending.pop_front() {
        match apply_item(net, d, physics.as_deref_mut(), &item) {
            Applied::Done => {}
            Applied::Blocked(key) => {
                net.stats.missing_assets = vec![key];
                net.client.pending.push_front(item);
                return;
            }
            Applied::Lost(code) => {
                lose_step(net, code);
                return;
            }
        }
    }
}

enum Applied {
    Done,
    /// Waits for this asset key.
    Blocked(u64),
    /// Lost step, with the agent code of the problem.
    Lost(&'static str),
}

fn apply_item(
    net: &mut DestructionNet,
    d: &mut Destruction,
    physics: Option<&mut PhysicsWorld>,
    item: &ClientItem,
) -> Applied {
    match item {
        ClientItem::Snapshot(instances, header) => {
            if let Some(missing) = instances.iter().find(|i| !net.assets.contains_key(&i.asset)) {
                return Applied::Blocked(missing.asset);
            }
            let assets: BTreeMap<DestructibleId, Arc<DestructibleAsset>> = instances
                .iter()
                .filter_map(|i| Some((i.state.id, net.assets.get(&i.asset)?.clone())))
                .collect();
            let mut snapshot = header.clone();
            snapshot.instances = instances.iter().map(|i| i.state.clone()).collect();
            match d.restore(physics, &snapshot, &|id| assets.get(&id).cloned()) {
                Ok(()) => {
                    net.stats.snapshots_applied += 1;
                    Applied::Done
                }
                Err(_) => Applied::Lost("destruction_bad_snapshot"),
            }
        }
        ClientItem::Op { seq, op } => apply_op(net, d, physics, *seq, op),
    }
}

fn apply_op(
    net: &mut DestructionNet,
    d: &mut Destruction,
    physics: Option<&mut PhysicsWorld>,
    seq: Option<u64>,
    op: &WireOp,
) -> Applied {
    let event = match op {
        WireOp::Define {
            id,
            asset,
            pose,
            seed,
        } => {
            let Some(asset_ref) = net.assets.get(asset) else {
                return Applied::Blocked(*asset);
            };
            return match d.world.define_at(*id, asset_ref.clone(), *pose, *seed) {
                Ok(()) => Applied::Done,
                Err(_) => Applied::Lost("destruction_bad_snapshot"),
            };
        }
        WireOp::Remove { id } => {
            d.remove_replicated(physics, *id);
            return Applied::Done;
        }
        WireOp::Instance { instance, .. } => {
            let Some(asset) = net.assets.get(&instance.asset) else {
                return Applied::Blocked(instance.asset);
            };
            return match d.restore_instance(&instance.state, asset.clone()) {
                Ok(()) => Applied::Done,
                Err(_) => Applied::Lost("destruction_bad_snapshot"),
            };
        }
        WireOp::Skip { next_seq, digest } => {
            return match d.world.skip_events(*next_seq, *digest) {
                Ok(()) => Applied::Done,
                Err(_) => Applied::Lost("destruction_stream_gap"),
            };
        }
        WireOp::Check { hash } => {
            if d.world.structure_hash() == *hash {
                net.stats.checks_passed += 1;
                return Applied::Done;
            }
            net.stats.checks_failed += 1;
            return Applied::Lost("destruction_hash_mismatch");
        }
        WireOp::Compact { id, tick, kind } => {
            let Some(seq) = seq else {
                return Applied::Lost("destruction_stream_gap");
            };
            match d.world.expand_event(seq, *tick, *id, kind) {
                Some(event) => event,
                None => return Applied::Lost("destruction_bad_event"),
            }
        }
        WireOp::Full { id, tick, kind, seed } => {
            let Some(seq) = seq else {
                return Applied::Lost("destruction_stream_gap");
            };
            wire::full_event(seq, *tick, *id, kind, *seed)
        }
    };
    if event.seq != d.world.event_count() {
        return Applied::Lost("destruction_stream_gap");
    }
    match d.world.apply_events(std::slice::from_ref(&event)) {
        Ok(_) => {
            net.stats.breaks_applied += 1;
            Applied::Done
        }
        Err(_) => Applied::Lost("destruction_bad_event"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(sync: u32, index: u16, total: u16) -> DestructionMessage {
        DestructionMessage::Snapshot(SnapshotPart {
            sync,
            index,
            total,
            ..SnapshotPart::default()
        })
    }

    fn stream() -> DestructionMessage {
        DestructionMessage::Stream(StreamBatch {
            ops: vec![WireOp::Check { hash: 7 }],
            ..StreamBatch::default()
        })
    }

    #[test]
    fn a_part_out_of_place_loses_step_instead_of_waiting_forever() {
        let mut net = DestructionNet::default();
        receive(&mut net, &part(4, 0, 3));
        receive(&mut net, &stream());
        receive(&mut net, &part(4, 2, 3));
        assert_eq!(net.client.phase, ClientPhase::Resyncing);
        assert!(net.client.need_resync);
        assert!(net.client.assembling.is_none());
        assert!(
            net.client.pending.is_empty(),
            "nothing without its snapshot may apply"
        );
        assert_eq!(
            net.stats.last_problem.as_deref(),
            Some("destruction_bad_snapshot")
        );
    }

    #[test]
    fn the_stream_behind_a_snapshot_applies_after_it() {
        let mut net = DestructionNet::default();
        receive(&mut net, &stream());
        assert!(net.client.pending.is_empty(), "no base yet");
        receive(&mut net, &part(1, 0, 2));
        receive(&mut net, &stream());
        receive(&mut net, &part(1, 1, 2));
        assert_eq!(net.client.phase, ClientPhase::Live);
        assert!(matches!(net.client.pending[0], ClientItem::Snapshot(..)));
        assert!(matches!(net.client.pending[1], ClientItem::Op { .. }));
        assert_eq!(net.client.pending.len(), 2);
    }

    #[test]
    fn a_new_snapshot_replaces_a_half_received_one_and_its_stream() {
        let mut net = DestructionNet::default();
        receive(&mut net, &part(1, 0, 2));
        receive(&mut net, &stream());
        receive(&mut net, &part(2, 0, 1));
        assert_eq!(net.client.pending.len(), 1);
        assert!(matches!(net.client.pending[0], ClientItem::Snapshot(..)));
    }

    fn big(id: u32) -> WireInstance {
        WireInstance {
            asset: 9,
            state: regia_physics::destruction::InstanceSnapshot {
                id: DestructibleId(id),
                pose: DestructiblePose::at([1.0, 2.0, 3.0]),
                seed: 5,
                detached: (0..4000).collect(),
                broken: (0..300_000).map(|b| b * 2).collect(),
            },
        }
    }

    #[test]
    fn pieces_of_a_big_instance_join_back_in_a_snapshot() {
        let whole = big(3);
        let pieces = wire::split_instance(&whole, 48 * 1024);
        assert!(pieces.len() > 6);
        let (first, rest) = pieces.split_at(pieces.len() / 2);
        let mut net = DestructionNet::default();
        let mut p0 = SnapshotPart {
            sync: 1,
            index: 0,
            total: 2,
            ..SnapshotPart::default()
        };
        p0.instances = first.to_vec();
        let mut p1 = SnapshotPart {
            sync: 1,
            index: 1,
            total: 2,
            ..SnapshotPart::default()
        };
        p1.instances = rest.to_vec();
        p1.instances.push(big(4));
        receive(&mut net, &DestructionMessage::Snapshot(p0));
        receive(&mut net, &DestructionMessage::Snapshot(p1));
        assert_eq!(net.client.phase, ClientPhase::Live);
        let ClientItem::Snapshot(instances, _) = &net.client.pending[0] else {
            panic!("a snapshot");
        };
        assert_eq!(instances, &vec![whole, big(4)]);
    }

    #[test]
    fn a_piece_that_does_not_continue_its_instance_loses_step() {
        let pieces = wire::split_instance(&big(3), 48 * 1024);
        let mut net = DestructionNet::default();
        let part = SnapshotPart {
            sync: 1,
            index: 0,
            total: 1,
            instances: vec![pieces[1].clone(), pieces[0].clone()],
            ..SnapshotPart::default()
        };
        receive(&mut net, &DestructionMessage::Snapshot(part));
        assert_eq!(net.client.phase, ClientPhase::Resyncing);
        assert_eq!(
            net.stats.last_problem.as_deref(),
            Some("destruction_bad_snapshot")
        );
    }

    #[test]
    fn a_gained_instance_in_pieces_is_one_item_and_a_skip_moves_the_count() {
        let mut net = DestructionNet::default();
        receive(&mut net, &part(1, 0, 1));
        let whole = big(8);
        let pieces = wire::split_instance(&whole, 48 * 1024);
        let last = pieces.len() - 1;
        let mut ops: Vec<WireOp> = pieces
            .into_iter()
            .enumerate()
            .map(|(k, instance)| WireOp::Instance {
                instance,
                more: k < last,
            })
            .collect();
        ops.push(WireOp::Skip {
            next_seq: 50,
            digest: 1,
        });
        ops.push(WireOp::Compact {
            id: DestructibleId(8),
            tick: 3,
            kind: regia_physics::destruction::CompactBreak::Bond { bond: 1 },
        });
        receive(
            &mut net,
            &DestructionMessage::Stream(StreamBatch {
                first_seq: 10,
                base_tick: 3,
                ops,
            }),
        );
        let items: Vec<&ClientItem> = net.client.pending.iter().collect();
        assert_eq!(items.len(), 4, "snapshot, instance, skip, break");
        assert!(matches!(
            items[1],
            ClientItem::Op { seq: None, op: WireOp::Instance { instance, more: false } } if *instance == whole
        ));
        assert!(matches!(items[3], ClientItem::Op { seq: Some(50), .. }));
    }

    #[test]
    fn sync_numbers_wrap_and_a_new_snapshot_still_replaces_the_old() {
        let mut net = DestructionNet::default();
        receive(&mut net, &part(u32::MAX, 0, 2));
        receive(&mut net, &part(0, 0, 1));
        assert_eq!(net.client.phase, ClientPhase::Live);
        assert_eq!(net.client.pending.len(), 1);
        assert_eq!(net.stats.last_problem, None);
    }

    #[test]
    fn parts_restart_the_wait_for_a_snapshot() {
        let mut net = DestructionNet::default();
        net.client.ticks = 50;
        receive(&mut net, &part(1, 0, 2));
        assert_eq!(net.client.last_request_tick, 50);
    }
}
