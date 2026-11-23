# 70 — Network model: process kinds, peers, authority, ownership and net messages

Status: proposed (ADR-1599). Plan item D2/D3 of `master-plan.md` § 12 (`regia_net` replication layer), rung v0.3.x (b) start,
v0.5.0 complete. Crate: `regia_net` (new, layer 3, next to `regia_framework`). This spec is the base the other `regia_net`
specs build on: 71 entity replication, 72 snapshot delta, relevance and priority, 73 prediction and reconciliation,
74 interpolation, 75 lag compensation, 76 dedicated server package, 77 session lifecycle.
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built (v0.5.0): `crates/regia_net` ships the transport and session layer and this spec's model on top of it: UDP
connections with reliable and unreliable channels, the handshake with join tokens, Noise NX encryption (ADR-1901), the
authoritative `TickDriver`, the endpoints `NetServer` / `NetClient` generic over a game's `NetGame`, `NetMode` with
client, listen and dedicated servers (`regia server`, `regia package --server`), sessions (spec 77), lobbies (spec 89),
the relay and join codes, the standalone `SnapshotEncoder`, and the replication, relevance, prediction, interpolation
and lag compensation of specs 71-75 (module table: `crates/regia_net/src/lib.rs`). Transports: raw UDP, the in-memory
loopback, and Steam sockets (`regia_steam::SteamSocket`). How to use it: `book/user-guide/networking.md`; agent pages
`book/agents/net.md`, `net-model.md`, `net-session.md`, `net-crypto.md`, `net-dedicated-server.md`, `net-lobbies.md`
and the other `book/agents/net-*.md`. The "Baseline" paragraph of § 1 describes the repo before the crate was built.

Builders read this file, the sibling specs 71-77 (they share `config/net.ron` and the vocabulary), and the ambient rule docs. Facts
this spec needs from elsewhere are restated where used: the transport channels in § 6.1, the codec trait in § 6.1, the player model
in § 4.

Source area (public-sources spec set, ADR-1526; behavior only): `runtime-networking/runtime-networking-overview`,
`runtime-networking/runtime-networking-remote-procedure-calls`. Re-derived in Regia terms from public industry sources
(§ 12); no wording, identifiers, settings names or command names carried over.

## 1. Purpose and scope

**Baseline.** `crates/regia_net` does not exist yet. Present in the repo and reused: `regia_framework::player` (`Player { index: u8,
kind: PlayerKind::{Local, Remote}, controller: Entity, device }`, `PlayerManager`, `PlayerController` with `possess`/`unpossess`;
`PlayerKind::Remote` is a placeholder with no networking behind it), the `FixedUpdate` schedule with `FixedTime` in `regia_app`, and
the agent method registry in `regia_agent`. The transport, handshake, peer health and conditioner are not in this repo either: they
come from the `ir-net` extraction (§ 1 below). Crate placement: `regia_net` is layer 3 (Gameplay) like `regia_framework`; the
builder adds `regia_net` to `layer_of` and the edges `regia_net -> regia_framework` and `regia_agent -> regia_net` to
`xtask/src/layers.rs` (the layer table lists same-layer edges explicitly). `regia_net` needs no `unsafe` (workspace `deny` holds).

Regia games are client-server. One process holds the true game state (the **server**); every other process (a **client**)
shows an approximation built from what the server sends. This spec fixes the shared vocabulary and four contracts every
other net spec relies on:

1. **Process kinds**: what a process is (offline, dedicated server, listen server, client) and how gameplay code asks.
2. **Peers and players**: how connections and the humans behind them are named.
3. **Authority, ownership and roles**: who may change an entity, who may send input for it, and how each process treats its copy.
4. **Net messages**: one-off calls between processes (events, requests), with direction, reliability, ordering, validation and
   flood limits.

The rule behind all four: **gameplay written for the network runs unchanged offline.** An `Offline` process is its own server,
so the same systems, messages and ownership checks run in single-player with no transport at all.

Relation to the `ir-net` extraction (`master-plan.md` § 3.5b, items D1/D2): that work supplies the transport, handshake and
tokens, the tick driver, congestion control, relay and the network conditioner. This spec and 71-77 define the layer above it.
Where the extraction already has a piece (for example peer health), the builder keeps the extraction's code and makes it meet
the behavior written here; a conflict is an ADR, not a silent choice. The extraction delivers these services, which the specs
assume: a `Transport` that moves byte packets per connection on four channels (`ReliableOrdered`, `ReliableUnordered`,
`Unreliable`, `UnreliableSequenced`) with fragmentation, acknowledgements and round-trip estimation; connect tokens and encryption;
and the fixed tick driver. Decision (ADR-1585): this spec adds no dependency. UDP uses `std::net`; any crypto or QUIC crate the
extraction needs gets its own `docs/03` row and ADR in the extraction lane, not here. Encryption is ADR-1901.

Not in scope: peer-to-peer and lockstep topologies (DO27's own model stays DO27's), host migration, a client that is
authoritative for gameplay state (only the narrow movement tolerance in spec 73 § 6.8), cross-process call return values (use a
request message and a reply message), scripting-language bindings.

## 2. Terms

| Term | Meaning |
|---|---|
| Server | The process holding authoritative state. Either a dedicated server (no local players, no rendering, no audio) or a listen server (hosts and also has local players). An offline process acts as a server with no connections. |
| Client | A process connected to one server. It never runs authority logic for replicated entities. |
| Peer | One end of a connection, as seen by the server: the server itself or one connected client process. |
| Player | A human participant (glossary): a peer plus a local index, because one client process can carry several split-screen players. |
| Replicated entity | An entity the server sends to clients (spec 71). Only replicated entities have a `NetId`. |
| Authority | The process allowed to change an entity's replicated state. For a replicated entity this is always the server. |
| Owner | The peer that may send input and to-server messages for an entity (its player's character, the weapon it holds). Ownership is not authority. |
| Role | How this process treats its copy of an entity: `Local`, `Authority`, `Predicted` or `Proxy` (§ 5). |
| Net message | A typed one-off call from one process to another, attached to a replicated entity or to the session. |
| Server tick | One fixed step of the server simulation (`FixedTime`, default 60 Hz). Every replicated fact is stamped with the tick that produced it. Type `NetTick(pub u32)`, counted from 0 at session start (it lasts 828 days at 60 Hz; a session that reaches `u32::MAX` ends with `server_shutdown`). |

## 3. Process kinds

Naming note (ADR-5647): the built type is `regia_net::NetMode` (ADR-1643 shipped it before ADR-1661's rename reached the
builder), with exactly these variants, helpers and run conditions. Whether to rename it to `ProcessKind` is open for the owner
and counsel.

```rust
/// What this process is. A resource; changes only through the session (spec 77).
#[derive(Resource, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessKind {
    /// No networking. The process is its own server and accepts no connections.
    #[default]
    Offline,
    /// Hosts a session; no local players, no rendering, no audio (spec 76).
    DedicatedServer,
    /// Hosts a session and has local players.
    ListenServer,
    /// Connected (or connecting) to one server.
    Client,
}

impl ProcessKind {
    pub fn is_server(self) -> bool;          // Offline | DedicatedServer | ListenServer
    pub fn is_client(self) -> bool;          // Client
    pub fn has_local_players(self) -> bool;  // Offline | ListenServer | Client
    pub fn accepts_connections(self) -> bool;// DedicatedServer | ListenServer
}
```

- Run conditions for systems: `on_server()` (`is_server`), `on_client()`, `with_local_players()`, `offline()`. They read
  `ProcessKind` and are the only supported way to gate a system by process kind; code never inspects the transport.
- `ProcessKind` is set by the session (spec 77) before the first fixed step of a session and stays fixed until the session ends.
  A change of kind is a session end plus a new session; systems never see a kind change mid-tick.
- Default: `Offline` in client builds (the game starts single-player until it hosts or connects). A `server` build (spec 76) has
  its host set `DedicatedServer` before the first tick; `ProcessKind::default()` is not used there.
- Build features: the `regia` umbrella gains a `net` feature (enables `regia_net`) and a `server` feature (spec 76: dedicated
  server package, no render, audio or window). A client build contains the server code paths, because listen servers and
  offline play need them; stripping server code from client builds is out of scope.

## 4. Peers and players

```rust
/// One end of a connection. `PeerId::SERVER` is the server process itself (also the host of a listen server).
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct PeerId(pub u32);
impl PeerId { pub const SERVER: PeerId = PeerId(0); }

/// A human participant: the peer plus a split-screen index on that peer.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct NetPlayerId { pub peer: PeerId, pub local: u8 }

/// On the existing `Player` entity (`regia_framework::player`), next to `Player { index, kind, controller, .. }`, on every
/// process that knows the player.
#[derive(Component, Reflect, Debug, Clone, Copy)]
pub struct NetPlayer { pub id: NetPlayerId }

/// The id of a replicated entity (spec 71 § 4.1). A component on every replicated entity, on every process.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct NetId(pub u32);

/// A server tick (§ 2) and a fractional server tick (interpolation and lag compensation, specs 74-75).
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, serde::Serialize, serde::Deserialize)]
pub struct NetTick(pub u32);
#[derive(Reflect, Debug, Clone, Copy, PartialEq, PartialOrd, Default, serde::Serialize, serde::Deserialize)]
pub struct NetTickFrac(pub f64);
```

- **Mapping to the existing player model.** `regia_framework::player` already has `Player { index, kind: Local | Remote, controller,
  device }` and `PlayerManager`. `regia_net` does not replace them: when a remote player joins (spec 77), the server (and each client,
  for every player it learns about) spawns a `Player` with `kind: Remote` through `PlayerManager` and adds `NetPlayer`. `Player.index`
  stays the process-local slot (it may differ between processes); `NetPlayerId` is the identity on the wire. The player's
  `PlayerController` entity is `Player.controller` and is replicated.
- Names in net types (`NetGroups`, hit-shape tags, stream kinds) use `regia_core::id::Name`.
- The server gives client peers ids from 1 upward in the order their handshake completes (spec 77), and never reuses an id
  within one session. A session that would need id `u32::MAX` refuses the join with `DisconnectReason::ServerFull`.
- Local players on a listen server and in an offline process use `PeerId::SERVER` with `local` 0..n.
- A client process learns its own `PeerId` in the welcome message (spec 77); until then `LocalPeer` (resource) is `None`.
- At most 8 local players per peer (`local` 0..=7); at most 256 players in a session (`Player.index` is a `u8`); a join that declares more is refused with `DisconnectReason::TooManyLocalPlayers`.

## 5. Authority, ownership and roles

### 5.1 Authority

- Every replicated entity is created by the server and changed only by the server. A client that writes a replicated
  component on its copy changes only its own copy; the next state from the server overwrites it (spec 71 § 4). Prediction
  (spec 73) is the one sanctioned case of a client running ahead on its own copy, and it is always reconciled.
- Entities a client spawns itself are `Local`: they never replicate and the server never sees them (effects, UI, local
  debris).

### 5.2 Ownership

```rust
/// Server side: which peer owns this entity. Absent = owned by the server.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetOwner(pub PeerId);

/// Server side: borrow ownership (and, spec 72, relevance) from another entity that is not a transform parent,
/// for example a weapon held through an attachment socket.
#[derive(Component, Reflect, Debug, Clone, Copy)]
pub struct NetOwnerLink(pub Entity);
```

- **Resolution.** An entity's owner is found by walking up from it: its own `NetOwner` if present, else its `NetOwnerLink`
  target, else its `ChildOf` parent, repeated. The first `NetOwner` found decides. No `NetOwner` on the chain means
  owned by the server. The walk stops after 32 steps or on a cycle; such an entity is treated as server-owned and the server
  logs `net.owner_chain_broken` once per entity (warn).
- **Possession.** When the `PlayerController` of a player with a `NetPlayer` possesses a character (spec 14), the server sets the
  character's `NetOwner` to the controller's peer; unpossessing removes it. A controller entity always carries the
  `NetOwner` of its own peer, set by the session on join.
- **Replicated view.** Clients do not receive the `PeerId` of other entities' owners. Each client receives one bit per
  replicated entity: owned by me or not (`NetOwnedLocally` marker component, inserted and removed by `regia_net`).
- **Ownership changes** are server decisions and take effect at the tick they are made. The new owner's client sees
  `NetOwnedLocally` appear with the state of that tick; the old owner's client sees it removed at the same tick. Messages
  queued by the old owner and received after the change are dropped (§ 6.5).

### 5.3 Roles

```rust
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetRole {
    /// Not replicated: exists only in this process.
    Local,
    /// This process is the authority (the server copy, or any entity in an offline process).
    Authority,
    /// A client copy that this client owns and simulates ahead of the server (spec 73).
    Predicted,
    /// A client copy driven only by replicated state, shown through interpolation (spec 74).
    Proxy,
}
```

- `regia_net` inserts and maintains `NetRole` on every replicated entity. Entities without `NetRole` are `Local` (the
  component is not added to purely local entities, to keep memory down).
- On the server and offline: every replicated entity is `Authority`.
- On a client: an owned entity whose prefab enables prediction (spec 73 `Predicted` setting) is `Predicted`; every other
  replicated entity is `Proxy`. When ownership moves, the role changes at the same tick.
- Helper queries for gameplay: `has_authority(&NetRole)`, `is_locally_controlled(&NetRole, Option<&NetOwnedLocally>)`.
  The typical branch is "authority runs the rule, the locally controlled copy plays the instant feedback, proxies only show".

## 6. Net messages

### 6.1 Declaring a message

```rust
/// A message type. Encoded with the net codec (`ReplicateCodec`, spec 71 § 3), not serde, so `Entity` fields travel as `NetId`
/// (an entity that is not replicated travels as "none" and arrives as `Entity::PLACEHOLDER`). Registered with
/// `app.add_net_message::<M>()`.
pub trait NetMessage: ReplicateCodec + Send + Sync + 'static {
    const DIRECTION: Direction;
    const CHANNEL: Channel = Channel::ReliableOrdered;
    /// Inbound flood limit per sending peer (§ 6.6). `None` = the session default.
    const RATE_LIMIT: Option<RateLimit> = None;
    /// Always admitted by the flood guard (§ 6.6) but still counted. For tiny, essential messages only.
    const ALWAYS_ADMIT: bool = false;
    /// Client side, for messages whose target is a proxy: hold the message until the proxy is shown at its tick (§ 6.5 item 3).
    const ALIGN_TO_VIEW: bool = false;
    /// Broadcast only: do not deliver to the target's owner when the owner predicts it (spec 73 § 8).
    const SKIP_PREDICTING_OWNER: bool = false;
    /// Server side, to-server messages only: decide before any handler runs (§ 6.4).
    fn validate(&self, _cx: &ValidateContext) -> Verdict { Verdict::Accept }
}

pub enum Direction {
    /// Client to server. Sent on an entity the sender owns, or on the session.
    ToServer,
    /// Server to the owning client of the target entity, or to one named peer for session messages.
    ToOwner,
    /// Server to every client for which the target entity is relevant (spec 72), and to the server itself.
    Broadcast,
}

pub enum Channel { ReliableOrdered, ReliableUnordered, Unreliable, UnreliableSequenced }
pub enum Verdict { Accept, Reject(&'static str), Kick(&'static str) }
pub struct RateLimit { pub count: u32, pub per_ticks: u32 }
pub struct ValidateContext<'a> { pub from: PeerId, pub target: Option<Entity>, pub world: &'a World, pub tick: NetTick }
```

`ReplicateCodec` (defined in spec 71 § 3) is the net codec trait: `fn encode(&self, w: &mut BitWriter, cx: &CodecContext)` and
`fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError>`, derived with `#[derive(Replicate)]` for structs of
supported field types. A builder of this spec implements `NetMessage` against that trait; until spec 71's derive lands, tests use
hand-written impls.

`#[derive(NetMessage)]` with `#[net(direction = "to_server", channel = "unreliable", rate = "10/60")]` generates the trait.
The protocol hash (spec 77 § 4) includes every registered message type's path, direction, channel and field layout, so a client
and server that disagree on any message cannot join each other.

### 6.2 Sending and receiving

- Send: `NetSender` system parameter: `send_to_server(target, msg)`, `send_to_owner(target, msg)`, `broadcast(target, msg)`,
  and session forms `send_session_to_server(msg)`, `send_session_to_peer(peer, msg)`, `broadcast_session(msg)`. `target` is
  an `Entity` that must be replicated; otherwise the call returns `Err(NetSendError::NotReplicated)`. A direction that does
  not match `M::DIRECTION` is a compile error (the methods are bounded by per-direction marker traits). The mode is only known at
  run time, so a server-only send on a client returns `Err(NetSendError::NotServer)` (and a client-only send on a dedicated server
  `Err(NetSendError::NotClient)`), logged once per message type.
- Receive: handlers are systems that read `NetMessages<M>` (an event reader). Each item carries `msg`, `target: Option<Entity>`,
  `from: PeerId`, `sent_tick` (the sender's tick) and `late: bool` (§ 6.5).
- No return value. Request/reply is two message types; the reply carries a request id chosen by the sender.

### 6.3 Where a message runs

The rules below make offline play and the listen-server host behave exactly like a remote client from the gameplay code's point of
view. "Runs" means the handler sees the message in its `NetMessages<M>`.

| Direction | Sent by | Target owner | Runs on |
|---|---|---|---|
| ToServer | client | that client | server |
| ToServer | client | another peer, or the server | nowhere: dropped by the server, counted as `not_owner` |
| ToServer | server (offline, listen host) | any | server, at `NetSet::Apply` of the next tick (§ 8.1) |
| ToOwner | server | a client peer | that client only |
| ToOwner | server | the server (unowned, or a host player) | server, at `NetSet::Apply` of the next tick |
| ToOwner | client | any | nowhere: `send_to_owner` returns `Err(NotServer)` on a client |
| Broadcast | server | any | server and every client where the target is relevant at the send tick (spec 72) |
| Broadcast | client | any | that client only (a local cosmetic broadcast; never sent) |

- A Broadcast to a client where the target is not relevant is not stored for later: if the entity becomes relevant later,
  the client never sees the old broadcast. Persistent facts belong in replicated state, not in messages.
- Session messages (no target) use the peer directly: `ToServer` from any client runs on the server; `ToOwner` names a peer;
  `Broadcast` runs on every client in `InGame` (spec 77) and on the server.

### 6.4 Validation

- The server runs `validate` for every to-server message before any handler, with `ValidateContext { from, target, world: &World,
  tick }` (read-only world).
- `Accept` delivers. `Reject(reason)` drops the message, increments the per-type `rejected` counter and logs at debug (with a
  per-peer log cap of 10 lines per second). `Kick(reason)` drops it and disconnects the peer with
  `DisconnectReason::InvalidMessage(reason)` (spec 77).
- Decoding failure (malformed bytes) is always a kick with `DisconnectReason::MalformedPacket`: a correct client never sends it.

### 6.5 Ordering and delivery guarantees

Messages ride the transport channels of T2.3. Regia makes these guarantees, and only these:

1. **Reliable** messages (`ReliableOrdered`, `ReliableUnordered`) arrive exactly once while the connection lives. `ReliableOrdered`
   messages run in send order **per connection**, across all target entities. `ReliableUnordered` messages run in arrival order.
2. **Unreliable** messages arrive at most once and may be lost or reordered. `UnreliableSequenced` drops any message older than the
   newest one already received of the same type and target.
3. **Against state.** A client applies everything from one server tick in this order: spawns, then replicated component state,
   then messages stamped with that tick, then despawns (spec 71 § 4). A message therefore sees the state of the tick it was sent in,
   or newer. If newer state of the target has already been applied when the message arrives (its tick is late), the message still
   runs, with `late = true`. This holds for authority and predicted copies. A **proxy** is shown at the interpolation tick (spec 74),
   about one delay behind its newest state: a message targeting a proxy runs on arrival by default (ahead of what the player sees);
   types with `ALIGN_TO_VIEW = true` are held on the client until `interp_tick >= sent_tick` (at most `max_delay_ms` of spec 74), so
   an explosion message lines up with the shown pose.
4. **Unresolved targets.** A message whose target entity is not spawned yet on the receiver waits up to 32 server ticks for the
   spawn. After that it is dropped and counted as `unresolved`. A message whose target was despawned is dropped and counted.
5. **Ownership change.** The server drops a to-server message whose sender no longer owns the target at the tick the message is
   processed (`not_owner`).
6. **Different messages, different ticks:** no guarantee of order between unreliable messages, between different channels, or
   between messages and state of a different tick. Code that needs one state change and one event to be seen together puts both in
   state, or both in one message.

Queue bounds:

| Queue | Bound | On overflow |
|---|---|---|
| Reliable send queue per connection (unacknowledged + waiting) | 4,096 messages or 1 MiB | Disconnect the peer with `DisconnectReason::ReliableOverflow`; reliable data cannot be dropped. |
| Unreliable send queue per target entity per connection | 16 messages | Drop the oldest; count `dropped_overflow`. |
| Broadcasts per server tick | 1,024 | Drop further broadcasts this tick; count `dropped_overflow`; warn once per second. |
| Message size | 64 KiB encoded | `send_*` returns `Err(NetSendError::TooLarge)`; nothing is sent. Larger data uses a stream (spec 77 § 8). |

Gameplay guidance written into the guide page: prefer replicated state over messages for anything that must be true later;
send per-tick input through the prediction command stream (spec 73), not through reliable messages; give any reliable message
that a button can trigger a rate limit.

### 6.6 Flood guard

Inbound protection on the server, one `FloodGuard` per connected peer:

- **Per message type:** `RateLimit { count, per_ticks }` is a token bucket that gains `count / per_ticks` tokens each tick, capped at
  `count`. A message that finds no token is dropped, counted as `rate_limited`, and not delivered or validated. Default per type: 120
  tokens per 60 ticks.
- **Per peer:** a cap of 512 inbound messages per tick and 256 KiB per second over all types.
- **Escalation.** Each peer has a `FloodState`: `Clear` or `Slowed`. A peer becomes `Slowed` when it passes the per-peer cap in any
  tick, or when more than half of its messages in the last 60 ticks were `rate_limited`. While `Slowed`, every per-type rate is
  halved: an even `count` halves, an odd `count` keeps its count over twice `per_ticks` (ADR-5580). A `Slowed` peer returns to `Clear` after 300 ticks with no breach. A peer that stays `Slowed` and keeps breaching for 600
  ticks is disconnected with `DisconnectReason::Flood`.
- **Stall forgiveness.** If the previous frame's simulation step reached `stall_forgive_ms` (a level load, a debugger stop;
  `Time::sim_delta`, never the wall clock, ADR-4772), the next frame's messages are not counted toward escalation: the queued
  burst comes from the stall, not from the peer. Under `TimeUpdateStrategy::Automatic` the simulation frame delta is clamped to
  `MAX_DELTA` (250 ms), so a frame that hit the clamp reaches any threshold of 250 ms or more (it may have been far longer); a
  threshold below the clamp still needs the simulation step to actually reach it. `0` disables forgiveness
  (audit fixrev-15-k net3-determinism M3, ADR-5444). The simulation step is the clamped frame delta times `Time::time_scale`,
  so the rule scales with it: in slow motion (scale 0.5) a stalled frame reaches at most 125 ms and the default 250 ms threshold
  never forgives, while at scale 2.0 an ordinary 125 ms frame forgives. A game that runs slow motion online sets
  `stall_forgive_ms` to the scaled clamp (`MAX_DELTA` × scale) for that stretch (audit rev-kimi-17 fixrev-15-k finding 5, ADR-5880).
- `ALWAYS_ADMIT` types skip the token bucket and the per-peer cap, and never count toward escalation, but appear in the counters.
- **Inbound queue (ADR-1892).** In front of this guard, a peer's queue between two ticks holds 8 × the per-peer cap (4096 messages). A
  frame that does not fit is shed undecoded, `ALWAYS_ADMIT` types included, and counts as a breach; two seconds of overflow in a row
  disconnect the peer with `Flood`. `net.peer_flood` reports the shed count as `inbound_shed`. The flood guard's second follows the real
  fixed tick rate, not an assumed 60 Hz.
- Every drop and every state change is a counter readable through `net.messages` and `net.peer_flood` (§ 9); a state change also
  logs at info with the peer id.

### 6.7 Hostile input at the transport (ADR-7892)

`NetServer` (the transport under the net model; `ServerEvent`, `NetServer::advance_tick`) decodes each message a connected peer
sends: game messages, input bundles (`InputBundle`, decoded with the game's `NetGame::Input` codec) and its own lanes.

- **Over-cap input is fatal.** An input bundle whose decode returns `WireError::TooLong` (more than `MAX_BUNDLE_INPUTS` inputs,
  or a field of the game's input codec, such as a command tail, past its cap) disconnects its sender at once with
  `DisconnectReason::ProtocolError`, as the connection layer does for an over-long field. Nothing after it in the record is read;
  no other peer is affected. A correct client never sends it.
- **Other undecodable messages score.** An input bundle that is truncated, holds a value the codec refuses, or leaves bytes over
  after the bundle (the decode ends with `WireReader::finish`), a game message that fails to decode or leaves bytes over, and a
  message on an unknown lane each add one malformed point. A record (one authenticated packet) earns one point of credit only when
  every message in it decoded, so one bad message in each otherwise valid record still adds a point per record. At 1024 points
  the peer is disconnected with `DisconnectReason::MalformedPacket`.
  *Changed by ADR-8181 (spec 120 § 11.2):* no record earns credit any more; the score leaks 4 points a second instead, so
  bad/good alternation adds up too. Connected peers are also policed for rate before the open (spec 120 § 11.3).
- **The host hears each refusal.** Every refused message raises `ServerEvent::Rejected { peer, kind, error }` before any
  `PeerLeft` it causes. `kind` is a `RejectKind`: `input_too_long` (fatal), `input_malformed`, `message_malformed` or
  `unknown_lane`. Nothing of a refused message reaches the game or the tick driver. A host with a stricter policy acts on the
  event itself, for example `NetServer::kick` on the first refusal.
- Tests: `regia_net/tests/input_rejection.rs` over real UDP loopback (over-cap disconnects only its sender; 1,100 malformed
  records drop the sender; trailing bytes are refused; the event names the peer), and the seeded hostile streams of
  `regia_net/tests/fuzz.rs` `over_cap_input_tails_drop_only_their_senders`.

### 6.8 Input readiness for a chosen set (ADR-7892)

`TickDriver::all_ready_for(tick)` asks whether every registered player has input for `tick`. A game that steps on a set it
chooses, for example its live players while a cleanly leaving peer stays registered until its committed removal tick, asks
`TickDriver::ready_for(tick, peers)` (every peer of the set has input; an unregistered peer is not ready; an empty set is not
ready) or `TickDriver::has_input(peer, tick)`. Both only read: nothing is taken, no miss is counted, and the engine stays the only
input buffer. A peer stays registered until it leaves (`ServerEvent::PeerLeft`) or the host kicks it.

### 6.9 Endpoint input bundles and inputs larger than a datagram (ADR-7896)

`NetClient::send_input(tick, input)` (the endpoint layer under the net runtime) sends the game's per-tick input to the server's
`TickDriver` with redundancy: every send repeats up to `ClientConfig::input_redundancy` remembered inputs (1..=32).

- **Budget.** One unreliable message fits one packet: `LinkConfig::max_packet_bytes` less the endpoint header (34 bytes sealed,
  9 plain), the 9-byte packet header, the 3-byte record header and the lane byte. At the default 1200-byte cap on a sealed link a
  bundle may take 1153 bytes (`NetClient::input_bundle_budget`), the 9-byte bundle header (`first_tick u64 | count u8`) included.
- **Bundling.** Each send cuts the remembered history, newest first, into bundles of consecutive inputs that each fit the budget.
  The first bundle ends at the newest input; when the whole history fits (small inputs) it is the only one, exactly as before.
  Otherwise the older inputs follow in more bundles, at most `MAX_INPUT_BUNDLES_PER_SEND` (4) datagrams per send, so a stream of
  large inputs keeps several copies of each input in flight (548-byte inputs: two per bundle, eight ticks covered per send).
- **Inputs larger than a datagram.** An input whose own encoding does not fit a bundle travels on the reliable channel as one
  message, `lane 3 | tick u64 | input`, which the channel fragments and resends like any reliable message. It arrives intact and
  on its tick, one resend timeout late when a fragment is lost; it waits behind earlier reliable messages and holds later ones
  back while it is resent. The bundles skip it, so the inputs older than it keep their redundancy. Encoded inputs over
  `MAX_RELIABLE_INPUT_BYTES` (16 KiB) are refused by the client with `SendError::TooLarge`.
- **History.** The history changes only when the newest input was queued. A refused send (`TooLarge`, `Backpressure`) leaves it
  exactly as it was, so the next send is not affected.
- **Server.** Lane 3 is accepted only on the reliable channel and only from players. The length is checked before the game's
  `NetCodec::decode` runs, and the input must decode with no bytes left over. The § 6.7 rules apply to it: an input over the
  limit (or whose codec reports `WireError::TooLong`) is `RejectKind::InputTooLong` and disconnects its sender with
  `ProtocolError`; one that does not decode or leaves bytes over is `InputMalformed` and scores one malformed point; lane 3 on the
  unreliable channel is an `UnknownLane`. Each raises `ServerEvent::Rejected` for that peer.
  An accepted input goes through `TickDriver::push_input` like a bundled one: past ticks, duplicates and ticks beyond
  `ServerConfig::input_buffer_ticks` are ignored, and `all_ready_for` treats it like any other input. Reassembly memory is bounded
  by `LinkConfig::max_reliable_message_bytes` as for every reliable message; buffered reliable inputs by `input_buffer_ticks` ×
  16 KiB per peer.
- **Wire version.** The lane is `REGIA_NET_WIRE_VERSION` 3; a version 2 peer is refused at the handshake with `protocol_mismatch`.
- **Counters.** `EndpointTotals::input_bundles_trimmed` (client: bundles that held less than the whole history) and
  `reliable_inputs` (client: sent on the reliable lane; server: accepted) appear in `net.status` `totals`.
- Tests: `regia_net/tests/input_mtu.rs` over real sealed UDP loopback, and `server::tests` for forged fragment streams.

## 7. Files and formats

- No content file is introduced by this spec. Project settings for the whole of `regia_net` live in **`config/net.ron`**
  (`NetSettings`, `format_version: 1`), defined across specs 70-77; each spec owns its section. This spec owns `messages`:

```ron
NetSettings(
    format_version: 1,
    messages: (
        default_rate: (count: 120, per_ticks: 60),
        peer_cap_per_tick: 512,
        peer_bytes_per_second: 262144,
        slowed_recover_ticks: 300,
        disconnect_after_slowed_ticks: 600,
        stall_forgive_ms: 250,
        unresolved_wait_ticks: 32,
    ),
    // session: spec 77; replication: specs 71/72; prediction: 73; interpolation: 74; lag_compensation: 75
)
```

- Migration note: `config/net.ron` is unfrozen until the v0.5.0 release. Before the freeze every field added by specs 70-77 has a
  serde default, so older files load. At the freeze it is registered in `check-formats` and `regia upgrade-project` (spec 12 § 4a).
- The wire protocol is versioned separately: `NET_PROTOCOL_VERSION: u32` (starts at 1) plus the protocol hash (§ 6.1). Neither is a
  content format; a mismatch refuses the join (spec 77 § 4).

## 8. Determinism

- `PeerId` assignment depends only on handshake completion order; `NetId` assignment (spec 71) only on spawn order on the server.
- Message application order on a receiver is a pure function of the packets received: per tick, spawns, state, messages in channel
  then sequence order, despawns. Two runs fed the same packet log apply the same messages in the same order (this is what the
  conditioner replay test in § 10 checks).
- Offline mode runs no transport and no threads beyond the engine's own; net work runs at fixed points of each fixed tick (§ 8.1),
  so offline play stays deterministic under `enable_fixed_step`.

### 8.1 Where net work runs

Replication and messages are per **tick**, not per frame (`FixedUpdate` runs 0 to `max_substeps` times per frame, and a listen server
renders at a variable frame rate). The system sets, in order:

| Set | Schedule | Work |
|---|---|---|
| `NetSet::Receive` | `PreUpdate` | read packets from the transport; decode into per-tick queues; session state machine (spec 77) |
| `NetSet::Apply` | start of `FixedUpdate` | client: apply queued server ticks in tick order (spawns, state, messages, despawns); server: consume this tick's input commands (spec 73 § 5.2) and deliver queued to-server and local messages |
| `NetSet::Gameplay` | `FixedUpdate` | game systems: `.in_set(NetSet::Gameplay)` or `.after(NetSet::Apply).before(NetSet::Predict)` |
| `NetSet::Predict` | `FixedUpdate` | the predicted schedule (spec 73), after gameplay |
| `NetSet::Assign`, `NetSet::Gather` | end of `FixedUpdate` | server: assign `NetId`s, gather changes, store state versions (specs 71-72) |
| `NetSet::LagCompRecord` | end of `FixedUpdate` | server: record hit-shape history (spec 75) |
| `NetSet::Send` | end of `FixedUpdate` | on send ticks: relevance, priority, packing (spec 72); client: queue the input stream (spec 73) |
| `NetSet::Flush` | `PostUpdate` | hand packets to the transport |
| `NetSet::Interpolate` | `PostUpdate`, before transform propagation | client: proxy interpolation (spec 74) |

A frame that runs no fixed tick applies and sends nothing; a frame that runs three applies and sends three times.

## 9. Agent protocol surface

Guide page: `book/agents/net-model.md` (topic `net-model`): modes, roles, ownership, message rules with the § 6.3 table, the
gameplay guidance in § 6.5, and every error code below. `book/COVERAGE.md` gains a row per method.

| Method | Mode | Params | Returns |
|---|---|---|---|
| `net.status` | read | — | `{ mode, local_peer, tick, peers, players }` |
| `net.entity` | read | `{ entity }` | `{ net_id, role, owner (server only), owned_locally, relevant_to (server only, peer list) }` |
| `net.messages` | read | `{ type? }` | per message type: `sent, received, delivered, rejected, rate_limited, not_owner, unresolved, dropped_overflow, late` |
| `net.peer_flood` | read (server) | `{ peer }` | `{ state: clear\|slowed, breaches_last_second, last_breach_tick, inbound_shed }` |
| `net.set_owner` | play (server, dev builds) | `{ entity, peer \| null }` | `{ owner }` |

Errors follow the neighboring built modules (`xr.*`, `audio.voice`): code `F0002` with `data.reason` set to one of the lowercase ids
below, each documented on the guide page: `net_not_running` (no session and not offline), `net_not_server` (server-only method on a client), `entity_not_replicated`,
`entity_not_found`, `peer_unknown`, `message_type_unknown`.

## 10. Tests and acceptance

All tests run with an in-memory loopback transport (`regia_net::testing::LoopbackHub`: N processes as N `App`s in one test, packets
moved between them by the test at fixed steps, with optional loss, delay in ticks, duplication and reordering under a seeded RNG).
No sockets, no wall-clock.

1. **Offline parity.** A test game with a to-server "fire" message, a to-owner "hit confirmed" message and a broadcast "explosion"
   runs offline and as listen-server host; both runs produce the same event log, tick by tick.
2. **Mode gating.** Systems gated with `on_server()` run on offline, dedicated and listen processes and never on a client.
3. **Ownership resolution.** Owner found through `NetOwner`, through `NetOwnerLink`, and through two `ChildOf` levels; a 3-cycle and a
   40-deep chain both resolve to the server and log `net.owner_chain_broken` exactly once.
4. **Possession.** Possess sets `NetOwner`; the owning client sees `NetOwnedLocally` and `NetRole::Predicted` (if enabled) at the same
   tick; unpossess removes both; the other client never sees `NetOwnedLocally`.
5. **§ 6.3 table.** One test per row, asserting where the handler ran (and that it ran nowhere else).
6. **Validation.** `Reject` drops and counts; `Kick` disconnects with `InvalidMessage`; malformed bytes disconnect with
   `MalformedPacket`.
7. **Ordering.** With 20 % loss and reordering: reliable-ordered messages to two different entities arrive in send order; no reliable
   message is lost or duplicated over 10,000 messages; an unreliable-sequenced stream never delivers an older message after a newer one.
8. **Against state.** A message sent at tick T that writes a field which state at T also wrote: the handler sees state ≥ T; with the
   state packet delayed past the message, the message waits, then runs after the state (not before).
9. **Unresolved target.** A message for an entity whose spawn is delayed 10 ticks runs after the spawn; delayed 40 ticks, it is dropped
   and counted `unresolved`.
10. **Old owner.** Ownership moves at tick T; a to-server message from the old owner processed at T+1 is dropped as `not_owner`.
11. **Queue bounds.** 17 unreliable messages in one tick to one entity: the oldest is dropped and counted; a reliable queue over 4,096
    disconnects with `ReliableOverflow`; a 65 KiB message returns `TooLarge`.
12. **Flood.** A peer sending 3x its rate (two thirds rate-limited) gets drops and becomes `Slowed`; after 300 quiet ticks it is `Clear`; sustained flood for
    600 ticks while `Slowed` disconnects with `Flood`; an `ALWAYS_ADMIT` type is never blocked; a simulated 400 ms server frame forgives the
    next frame's burst.
13. **Replay determinism.** Record the packet log of test 7 and feed it to a fresh client twice: identical delivered-message logs.
14. **Agent.** `net.status`, `net.entity`, `net.messages`, `net.peer_flood`, `net.set_owner` round-trip in a loopback session; each
    error code is produced by one test; `cargo xtask agent-coverage` passes.

## 11. Platform notes and performance budget

- Linux, Windows and Android aarch64 run every mode except that Android does not host dedicated servers (spec 76). Listen servers on
  Android are allowed (local Wi-Fi play) and use the same code.
- macOS (testing only) builds and runs the loopback tests.
- Budget on the server (Desktop reference class of `06-testing-and-ci.md` § 3, 64 peers): message receive, validation and dispatch at 2,000 messages per tick under 0.25 ms;
  ownership resolution cached per tick (one walk per replicated entity whose parent, link or owner changed), never a walk per message.
  Spans: `net_messages_receive`, `net_messages_send`, `net_owner_resolve`.
- Client (Android reference class, `06` § 3): message dispatch under 0.05 ms at 200 messages per tick.
- Memory: per peer, flood guard under 2 KiB plus 32 bytes per registered message type.

## 12. Provenance

- Set spec ids used: `runtime-networking-overview`, `runtime-networking-remote-procedure-calls` (behavior only, ADR-1526).
- Public industry sources for the client-server model and message delivery: G. Fiedler, "Gaffer on Games" articles "What Every
  Programmer Needs to Know About Game Networking", "Reliability and Congestion Avoidance over UDP" and "Reliable Ordered Messages";
  Valve Developer Community wiki, the public multiplayer networking article; T. Ford, "Overwatch Gameplay Architecture and Netcode",
  GDC 2017.
- Regia's own design choices (not in any source): the per-tick apply order of § 6.5 item 3, the 32-tick unresolved wait, the queue
  bounds, the flood escalation thresholds and the stall-forgiveness threshold.

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 13. Owner decisions

None. Technical choices made in review (ADR-1585): no new dependency here; flood-guard state names and config keys are Regia's own
(`Clear`/`Slowed`, `ALWAYS_ADMIT`, `stall_forgive_ms`); the type is named `ProcessKind` (ADR-1661: the
review draft's name matched another engine's type for the same four values; the variant names are common industry terms).

## Addendum (ADR-5647): conformance audit, 2026-11-10

- **§ 11 owner cache.** Both message passes resolve each target entity's owner once per pass (`OwnerCache::owner`), clearing
  the cache at the start of the pass and after any kick; before, every message walked the chain.
- **§ 6.4 / § 6.5 logs.** Rejects log at most 10 lines per peer per second (second = tick / tick rate); the broadcast-per-tick
  overflow warns at most once per second. Every drop is still counted.
- **§ 6.5 item 3 hold cap.** An `ALIGN_TO_VIEW` hold ends after `max_delay_ms` (spec 74, converted to fixed ticks) even when
  the interpolation tick does not advance.
- **Built refinements recorded elsewhere:** `NetMessages<M>` is a per-tick resource, not an event reader, and sends can also
  fail with `WrongDirection`, `Unregistered` and `UnknownPeer` (ADR-1643); `VERSION` and `message_id` (ADR-2415); the inbound
  queue (ADR-1892); stall forgiveness from `Time::sim_delta` (ADR-4772); `config/net.ron` is `format_version` 2 (ADR-4854);
  `NET_PROTOCOL_VERSION` is 3 (ADR-1950, ADR-3315); `regia_net` sits in the Systems layer (ADR-1643). The server drops a to-server
  message whose target it does not know at once (`unresolved`); only clients wait the 32 ticks of § 6.5 item 4.
- **Built since (ADR-7212):** a joining peer's `NetPlayer` entity becomes a `Player` (`PlayerKind::Remote`) through
  `PlayerManager` at its welcome, with a `PlayerController` (`regia_framework::net_session_bridge`, feature `net`, when a
  `PlayerManager` resource exists); a client `NetSession` sets `LocalPeer` from the welcome. Possession sets and removes
  `NetOwner`: `PlayerController::possess` stays a plain method, and the bridge's `sync_possession_owners` (`PostUpdate`, before
  the next send) applies a remote player's controller change through `set_possession_owner` (the old pawn loses only the owner
  it was given). The layering is unchanged: `regia_framework` reaches `regia_net` through its optional `net` feature.
  Tests: `regia_framework/tests/net_session_players.rs` (two apps), `regia_net/tests/session_post05.rs`.
- **Not built:** § 10 test 5 is one test over all eight rows rather than one per row.

## Addendum (ADR-7905): netcode next generation, 2026-11-15

- **§ 1 transport claims are wrong at v0.5.0.** The transport has two physical channels (`connection.rs:36-41`); the four
  classes map onto them (`message.rs:55-58`, `ReliableUnordered` rides the one ordered stream) and nothing in the send path does
  congestion control. Spec 120 (`docs/specs/120-net-next-generation.md`) designs the transport that delivers both (§ 4 rate
  controller and pacing, § 5 streams and fragmentation, wire version 5); until its v0.6 lanes land, read § 1's list as the
  target, not the baseline.
- **§ 1 "Not in scope: lockstep, host migration"** is superseded by spec 120 § 7 (an engine lockstep mode with desync
  detection) and § 10.5 (host migration for listen servers and lockstep, v0.7, owner question Q1).


### Built: bounded outage retransmission (ADR-7950, N101)

The v0.5.x connection retains its RFC 6298 estimator and wire framing. A fragment's first
retry uses that RTO; consecutive retries without a newly acknowledged packet double the wait,
using a 250 ms repeated-timeout floor and an 8 s default cap. A newly acknowledged packet resets
all fragment backoffs; duplicate ACKs do not. `LinkConfig::resend_backoff_max_ms` controls the cap.

After more than two RTOs without feedback while reliable data waits, recovery starts with four
reliable-bearing packets per poll. Feedback from a newly sent round doubles the budget up to
`max_packets_per_poll`. A rolling limit additionally permits at most 32 reliable-bearing packets
per second, retained for at least one second after restored feedback. During recovery, empty ACKs
coalesce to keepalive cadence; at the default 100 ms interval the first restored second permits
at most 42 data/empty-control packets. Unreliable application traffic retains its own budget;
this is not the full rate controller of spec 120 § 4.2. Status exposes `link.recoveries` and
`link.recovering`; the common silence policy and interruption lifecycle are in spec 77 § 3.3.

### Built: validated address changes (ADR-8299, N103)

Transport: a record from a new address that authenticates (plain links: the session token and a newer packet sequence) makes
the address a candidate; its content is delivered, the endpoint keeps its validated address and sends a probe (a
ping packet with a packet number sent only there; transport v2, ADR-8387), and moves when a record from the candidate acknowledges
that number (RFC 9000 § 8.2, § 9.3; no wire change). Traffic to a candidate is capped at three times the bytes received from
it (§ 8.1). Trial opens are budgeted per (peer, candidate address). The client follows a server address change the same way. A
peer's move is not limited by `max_peers_per_ip`. Spec 120 § 10.4 (Built note) has the details.
