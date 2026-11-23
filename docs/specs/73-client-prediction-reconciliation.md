# 73 — Client prediction and server reconciliation

Status: proposed (ADR-1599). Plan item D3 (`master-plan.md` § 12: "prediction and reconciliation hooks"), rung v0.5.0. Crates:
`regia_net::predict` (input stream, tick sync, rollback), `regia_framework::character` (a refactor that exposes the movement step,
§ 9). Builds on specs 70 (roles), 71 (replicated components, teleports) and 72 (the server's state packets, which carry the input
acknowledgment).
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: `regia_framework::character` is implemented, spec records gaps: `CharacterInput { move_dir: Vec3, jump, crouch, sprint }`,
`CharacterState { velocity, grounded, mode, time_in_air, landed_this_frame, jumps_used, coyote_time_left, jump_buffer_time_left }`,
`MoveResult { position, grounded, ground_normal, ground_angle, hits }`, the `CollisionQuery` trait (`move_shape(&self, world, entity,
position, radius, half_height, delta, ...)`), `RapierCharacterQuery`, `RootMotionDriven`, and the private `FixedUpdate` system
`movement_step`, which takes the `CharacterCollisionQuery` resource out of the world while it steps every character. No `regia_net`
code exists; `InputRecording` (`regia_framework::input_recording`) is separate and unchanged.

Builders read only this file and specs 70-77 (restated here: `NetRole::{Authority, Predicted, Proxy}` and `NetOwnedLocally` from 70 § 5,
teleport bit and the `Transform` fast path from 71 § 9, the state packet's input acknowledgment from 72 § 7).

Source area (public-sources spec set, ADR-1526; behavior only): `runtime-networking/runtime-networking-movement-prediction`.
Its acceptance list was used to find edge cases (lost input, clock abuse, correction pacing, teleports, moving bases, animation-driven
motion). Regia's design is a general input-command and rollback model from public sources (§ 16), not a movement-component port; no
wording, identifiers, settings or numeric defaults carried over.

## 1. Purpose and scope

With a 100 ms round trip, a client that waits for the server before moving its own character feels broken. Prediction lets the owning
client simulate its own entities immediately from its own input, while the server stays the authority:

1. The client stamps each fixed step's input with a tick and sends it, redundantly, to the server.
2. The client runs the same gameplay step locally for that tick and remembers the result.
3. The server runs the step for that tick with the received input and sends back its state for the tick.
4. The client compares; if they differ, it rewinds to the server's state for that tick and re-runs the steps since, with the inputs it
   remembered (**reconciliation**). The visible pose blends out the jump.

In scope: game-defined input types, the input stream and its redundancy, the client's lead over the server and its adjustment, the
server's per-player input buffer, the predicted schedule, prediction history, mismatch tests, rollback and resimulation, correction
smoothing, teleports, moving bases, animation-driven motion, predicted spawns, first-time-only effects, and an opt-in client position
tolerance.

Not in scope: prediction of other players' entities (they are proxies, spec 74), server-side rewind for hit checks (spec 75),
deterministic lockstep, rollback of non-predicted entities.

## 2. Terms

| Term | Meaning |
|---|---|
| Input command | A game-defined value: one player's input for one tick (`impl NetInput`). |
| Client tick | The tick the client is predicting. It runs ahead of the latest server tick it has received by the lead. |
| Lead | How many ticks the client runs ahead, so its input for tick T reaches the server before the server simulates T. |
| Predicted component | A replicated component that the owning client simulates ahead (registered with `predict::<C>()`). |
| Predicted schedule | The set of fixed-step systems that run for predicted entities on the owning client, and normally on the server. |
| Prediction history | Per predicted entity, the predicted component values after each client tick, kept for comparison. |
| Correction | A mismatch between the server's state for tick S and the client's history for tick S, followed by rollback and resimulation. |
| Input buffer | Server-side, per player: received input commands keyed by tick, waiting to be consumed. |

## 3. Input commands

```rust
/// One player's input for one tick. Small and `Copy`-like; delta-encoded against the previous command in a packet.
pub trait NetInput: ReplicateCodec + Clone + Default + PartialEq + Send + Sync + 'static {
    /// What the server uses when a tick's command is missing (§ 5.2). The default repeats the last command unchanged;
    /// input types with one-shot fields (`jump`) override it to clear them.
    fn on_missing(last: &Self) -> Self { last.clone() }
}

impl NetInputAppExt for App { fn add_net_input<I: NetInput>(&mut self) -> &mut Self; }

/// Written by the game's input system on the owning client each fixed tick, before the predicted schedule.
#[derive(Component)]
pub struct InputCommand<I: NetInput> { pub tick: NetTick, pub value: I }
```

- One input type per game (`add_net_input::<I>()` once; a second call is a startup error). Each local player has its own stream.
- The game fills `InputCommand<I>` on its controller entity from `regia_input` actions in `FixedUpdate` before `NetSet::Predict`.
  On the server, `regia_net` writes the consumed command into the same component, so the game's step systems read input the same way
  on both sides.
- Offline and on a listen-server host, the local command is consumed directly at its tick: no stream, no lead, no rollback.

## 4. The input stream

- Every client tick, the client sends one unreliable packet (or rides the next outgoing packet) holding, for each local player, every
  command not yet acknowledged by the server, newest first, up to `prediction.input_redundancy` (default 16) commands. Commands after the
  first are delta-encoded against the next newer one, so an idle player's 16 commands cost a few bytes. A burst of up to 15 consecutive lost
  packets therefore loses no input.
- The server's state packets (spec 72 § 7) acknowledge the newest input tick it has received for each player and report the **buffer
  margin**: how many ticks early the newest command arrived, smoothed over the last 30 commands.
- The client keeps its sent commands in an input history ring of 128 ticks and drops entries at or below the acknowledged tick.
  Refined (ADR-1793 (5)): commands are sent only above the acknowledged tick but kept until the newest applied server state
  passes them, because a rollback from state S re-runs S+1 .. and the acknowledged tick is usually past S.

## 5. Ticks and the lead

### 5.1 Client lead

- The client knows the server tick of the newest state packet and the round-trip estimate from the transport. Its target lead is
  `ceil(rtt / 2 / step) + margin_target` ticks, where `margin_target` defaults to 2 ticks and grows by 1 for every 4 ms of jitter
  (standard deviation of the one-way delay estimate), up to 8.
- **Small error** (the reported buffer margin is off its target by at most `prediction.dilation_window_ticks`, default 8): the client adjusts
  its fixed step length by up to ±5 % (`FixedTime.hz` scaled) until the margin is back on target. Gameplay `dt` stays the nominal step; only
  how often steps run changes. Built as inserted or skipped predicted ticks (ADR-1793 (3)): one extra predicted tick (same input) or one
  skipped tick at most every `ceil(1 / max_dilation)` fixed steps, the same ±5 % of the tick rate; `FixedTime` itself is not changed.
- **Large error** (more than the window, or the margin is negative for 10 consecutive ticks): the client jumps its tick to the target and
  logs `net.lead_resync` at info. Inputs for skipped ticks are never sent; ticks jumped backward are not re-sent either (the client holds its
  input stream until its tick passes the newest acknowledged tick).
- **Stale margin (ADR-4915).** The reported margin is an average over the last commands, so after an adjustment part of it still
  describes the old lead. Both rules above read the margin expected once the adjustments in flight are reflected: an adjustment counts as
  reflected from the first command sent after it (a forward jump: its new tick; a backward jump: where the held stream resumes). A
  transport that has not sampled its round trip yet (`LinkStats::rtt_ms` 0) gives no RTT; the lead then uses its own estimate.
- The predicted tick is exposed as `NetClock { client_tick: NetTick, server_tick_estimate: NetTickFrac, interp_tick: NetTickFrac, lead,
  rtt_ms, jitter_ms }` (resource). `server_tick_estimate` and `interp_tick` are maintained by spec 74 § 4 (from packet arrival times);
  this spec reads them. The RTT and jitter come from the transport.

### 5.2 Server input buffer

- Per player, the server keeps received commands keyed by tick, up to 64 ticks ahead of its current tick.
- **On time.** At tick T the server consumes the command for T.
- **Missing.** No command for T: the server uses `I::on_missing(last)` and counts `input_missing`. The next state packet reports a negative
  margin, so the client raises its lead.
- **Late.** A command for a tick the server already simulated: dropped, counted `input_late`. It is never re-applied to past ticks.
- **Early.** A command more than 64 ticks ahead: dropped, counted `input_early`.
- **No clock abuse.** The server consumes exactly one command per player per tick, on its own clock, with the fixed `dt`. A client that sends
  commands faster than real time only fills its buffer until commands are dropped as early; one that sends them slower gets repeats. Neither
  can move its character faster than the server's tick rate allows. There is no client-supplied time step to validate.
- Flood limits of spec 70 § 6.6 apply to input packets with their own budget (`prediction.max_input_packets_per_tick`, default 4 per player).

## 6. Prediction and reconciliation

### 6.1 Registration

```rust
impl PredictAppExt for App {
    /// `C` must already be replicated (spec 71). `tolerance` decides what counts as a mismatch.
    /// `Transform` counts as replicated through the fast path (spec 71 § 9).
    fn predict<C: Component + ReplicateCodec + Clone>(&mut self, tolerance: Tolerance<C>) -> &mut Self;
}

/// System parameter for predicted systems.
pub struct PredictionCx<'w, 's> { /* ... */ }
impl PredictionCx<'_, '_> {
    pub fn tick(&self) -> NetTick;                              // the tick being simulated
    pub fn is_resimulating(&self) -> bool;
    pub fn first_time(&self) -> bool;                           // § 8
    pub fn spawn_predicted(&mut self, player: NetPlayerId, bundle: impl Bundle) -> (Entity, PredictionKey);   // § 7, ADR-1793 (7)
    pub fn send_local_event<E: Message>(&mut self, e: E);        // dropped while resimulating, § 8
    // Built extras (ADR-1793 (1)): `is_server()`, `simulates(role)`, `owner_player(owner)`.
}

pub enum Tolerance<C> { Exact, Fn(fn(&C, &C) -> bool) }   // helpers: `Tolerance::position(0.01)`, `Tolerance::angle(0.01)`

/// Add fixed-step systems to the predicted schedule. They run on the server for every entity, and on the owning client for
/// `NetRole::Predicted` entities, once per tick and again during resimulation.
impl PredictedScheduleExt for App { fn add_predicted_systems<M>(&mut self, systems: impl IntoScheduleConfigs<ScheduleSystem, M>) -> &mut Self; }
```

- A prefab opts its entities into prediction with `Predicted` (reflected marker, saved in prefabs). An owned entity without it is a `Proxy` on
  its owner too.
- Predicted systems must read only predicted components, `InputCommand<I>`, `NetClock`, read-only world geometry (static colliders) and
  the poses of proxies (§ 6.5). They must not spawn non-predicted replicated entities, send messages directly, or touch resources other than
  through `PredictionCx` (§ 8). The builder adds a debug-build checker that records which component types the predicted schedule wrote and
  warns `net.predict_writes_unregistered` once per type.

### 6.2 Each client tick

1. Write `InputCommand<I>` for each local player (game system).
2. Run the predicted schedule for the client tick.
3. Store the predicted components of every predicted entity in its prediction history, keyed by the client tick (ring of
   `prediction.history_ticks`, default 64).
4. Queue the input for the stream.

### 6.3 On server state for a predicted entity

The state packet for tick S carries the server's predicted components for that entity after simulating tick S (the server's step for S
used the client's command for S, or a repeat).

1. If the client has no history for S (S is older than the ring, or newer than the client tick after a lead resync): set the predicted
   components to the server's values, clear the history, count `prediction_reset`, and continue from step 4 with nothing to re-run.
   Refined (ADR-1793 (6)): the reset moves every predicted entity back to S and re-runs S+1 ..= client tick with the inputs it knows, so
   the entity is not left a lead behind; only over `max_resim_ticks` does it snap without re-running (step 5). A predicted entity that is
   new to the client (spawned or made predicted after the clock started) has no history because it is new: it catches up through the
   same rollback and counts neither a reset nor a correction (ADR-2011).
2. Compare each predicted component of every predicted entity received for S with the history at S, using its tolerance.
3. All equal: drop history at or below S. Done. (This is the common case: no correction.)
4. Any mismatch: **rollback and resimulate**:
   - Write the server's values for S into every predicted entity's predicted components (not only the mismatched one: all predicted entities
     move back to S together, so they stay consistent with each other).
   - For each tick S+1 ..= client tick: set `InputCommand<I>` from the input history, run the predicted schedule with
     `PredictionCx::is_resimulating() == true`, overwrite the history entry for that tick.
   - Count `corrections`, record the largest position error, the number of ticks re-run.
5. At most `prediction.max_resim_ticks` (default 32) ticks are re-run in one frame. If more would be needed (a lead larger than that), the
   client snaps to the server values, clears history and counts `prediction_reset` instead.
6. Several state packets in one frame: only the newest S per entity is used.

### 6.4 Correction smoothing

- The simulated pose jumps to the corrected value at once. What is rendered does not: `regia_net` keeps a visual offset (the difference
  between the pre-correction and post-correction pose) on `NetVisualOffset` and decays it exponentially with half-life
  `prediction.smooth_half_life_ms` (default 40 ms; the decay runs per fixed step, `0.5^(step / half_life)`, ADR-1793 (8)). The render transform of the predicted entity's visual children is the simulated pose
  plus the offset; the camera may follow either (a `CameraRig` setting, default the smoothed pose). As built (ADR-7213):
  `CameraRig::follow` (`CameraFollow::Smoothed` | `Simulated`); `camera.set_mode { follow }` sets it.
- An error larger than `prediction.snap_distance_m` (default 2.0 m) or a rotation error over 90° snaps (offset cleared).
- A teleport bit (spec 71 § 9) on the server state always snaps, clears the prediction history and skips the comparison for that tick.

### 6.5 Proxies during prediction

- Proxies are not rolled back and not re-simulated. During resimulation, predicted systems that read a proxy's pose see its pose as of the
  frame (the interpolated pose, spec 74), the same as during the original prediction of the current tick. Interactions with proxies
  (pushing another player) can therefore mispredict and are corrected by the server; this is the expected limitation.
- Collisions against static geometry are exact on both sides.

### 6.6 Moving bases

- **Tick-driven movers.** Platforms, elevators and doors whose motion is a pure function of the server tick (`TickDrivenMotion { keys:
  Vec<(f32, Transform)>, start_tick: NetTick, mode: Once | Loop | PingPong }`: key times in seconds, poses interpolated linearly with
  rotation nlerp, evaluated at `(tick − start_tick) × step`; replicated, so both sides hold the same keys and start tick) are evaluated at the predicted tick on the client, so a
  character standing on one predicts exactly. The game should use them for anything players stand on.
- **Other bases** (a physics-driven boat): the character's predicted position is stored relative to the base's `NetId` while it stands on
  it (the movement step reports the base in `MoveResult::ground_entity`, § 9). At the start of every step, first run and re-run alike, the base-relative position is turned back into a world position with the base's latest known pose,
  which replaces the backend's carry. The comparison uses the base-relative position, because the base is late on the client (refined by ADR-1905).
  Corrections for a real difference, and on leaving or landing on a base, are expected and smoothed.

### 6.7 Animation-driven motion

- A character whose motion comes from animation root motion (montages: `regia_animation::{montage, montage_player}`, ADR-1465) is predictable only if the root motion is sampled from a
  replicated play state: montage, section and start tick. The step samples root motion at `tick × step − start × step`, never from frame
  time. Montage starts from predicted code record the start tick in a predicted component, so a resimulation reproduces them.
  Sampling is at full weight and without warp windows (ADR-1905).
- While animation root motion moves the character, it takes precedence over input-driven acceleration (the existing `RootMotionDriven` rule);
  gravity still applies in the falling mode.

### 6.8 Optional client position tolerance

- `prediction.trust_client_within_m: Option<f32>` (default `None`). When set, each input command also carries the client's predicted
  position after the tick. If the server's result differs by less than the tolerance, the client's reported position becomes the
  authoritative one for that tick (the velocity stays the server's). This hides tiny cross-platform floating-point drift at the cost of letting a cheater move up to the tolerance per second: the adopted distance draws on a per-player budget of
  the tolerance that refills at the tolerance per second (ADR-2686); the guide page says so plainly. Positions further away are corrected as usual.

## 7. Predicted spawns

- Predicted code may spawn an entity ahead of the server (a projectile): `PredictionCx::spawn_predicted(bundle)` returns a local entity and a
  `PredictionKey { player, tick, index }` (index counts predicted spawns of that player in that tick, in system order). The server's step,
  running the same code for the same tick, spawns the real replicated entity with the same key in its spawn record.
- When the client receives a spawn record with a key it has, it binds the record to its local entity (keeps the entity, inserts `NetId`,
  applies the server values with smoothing) instead of spawning a second one. The bound entity is then reconciled at S like every
  predicted entity (ADR-5649): when the history at S matches the server's values it keeps its own prediction for the client tick and no
  correction is counted; a mismatch rolls back and re-runs, and that rollback's visual offset is the smoothing.
- If no matching spawn record arrives by `acknowledged input tick ≥ key.tick + prediction.spawn_grace_ticks` (default 8), the prediction was
  wrong: the local entity is despawned and `NetPredictedSpawnRejected { entity, key }` is emitted (the game may play a fizzle).
- During resimulation, `spawn_predicted` for a key that already exists returns the existing entity instead of spawning again.
  A rollback to a tick before an unbound spawn's own tick keeps that spawn's record of its spawn tick, records no history for it
  before that tick, and restores it to that record when the re-run reaches the tick (ADR-1793 (7), ADR-5649 addendum).

## 8. Effects that must not repeat

- `PredictionCx::first_time() -> bool` is true while running a tick for the first time on this client, false during resimulation and on the
  server. Sounds, muzzle flashes and camera shakes triggered by predicted code are gated on it (or sent with `PredictionCx::send_local_event`, which
  drops the event during resimulation automatically).
- On the server, predicted systems run once per tick; their effects for other clients go through replicated state or broadcasts (spec 70),
  which the owner's client ignores for its own predicted entities when the broadcast type sets `SKIP_PREDICTING_OWNER` (spec 70 § 6.1).

## 9. Character movement refactor

- `regia_framework::character` exposes the per-entity movement step as a function the predicted schedule and the existing system both call:
  `step_character(world: &mut World, collision: &dyn CollisionQuery, entity: Entity, input: &CharacterInput, dt: f32) -> MoveResult`
  (the backend is a parameter because `movement_step` removes it from the world while stepping). It is the per-entity body of the private
  `movement_step`, which becomes a loop around it. No behavior change: the existing
  character tests must pass unchanged (rule 4). It takes `&mut World` because `CollisionQuery::move_shape` does; spec 14 § B4's
  "pure `(state, input, dt) -> state`" sentence is refined by this (ADR-1599): the step is deterministic given world geometry, state,
  input and `dt`, and writes only the entity's character components.
- `MoveResult` gains `ground_entity: Option<Entity>` (the entity the character stands on, from the collision backend; `None` for the stub
  backends). This is a new public struct field (host-visible: struct literals need `..Default::default()`; listed in the release notes).
- While resimulating, `regia_net` sets the resource `NetResimulating(true)`; `RapierCharacterQuery` then does not apply push impulses to
  dynamic bodies (they were applied the first time; the server applies them for real).
- `NetCharacterPlugin` provides a default input type `CharacterNetInput { move_dir: Vec2 (quantized 8 bits per axis), yaw: f32, pitch: f32
  (angle 16 bits), jump: bool, crouch: bool, sprint: bool }` with `on_missing` clearing `jump`, and a predicted system that writes the
  controlled character's `CharacterInput` from `InputCommand<CharacterNetInput>` before `step_character`. Games with richer input define
  their own `NetInput` type and the same mapping system.
- `CharacterState` and `Transform` are registered as predicted components by `NetCharacterPlugin` with one tolerance function over both:
  translation within 0.01 m, rotation within 0.01 rad, velocity within 0.05 m/s, `time_in_air`, `coyote_time_left` and
  `jump_buffer_time_left` within 1/240 s; `mode`, `grounded`, `jumps_used` and `crouched` exact; `ground_velocity` within the velocity tolerance except on a base;
  `landed_this_frame` not compared (it is recomputed). `regia_net`'s `EntityTolerance` carries the single function (ADR-1905).

### 9.1 Vehicles (added by ADR-3080)

A vehicle cannot re-run where a character does: its chassis lives in the rapier world, which does not go back for one entity. `regia_framework::net_vehicle` keeps the rules above and changes where the step runs. The server drives vehicles in the shared world in `PhysicsStep`, with the owner's command written to `VehicleInput` after `NetSet::Apply`, and the predicted schedule copies the result out. The owning client steps a private `VehicleSimWorld` (the chassis and copies of the static colliders) in the predicted schedule. A rollback writes `Transform` and `NetVehicleState`; the next step sees values it did not leave and puts the private world on them. Correction smoothing adds a rate limit to § 6.4, and vehicles use `NetVehicleVisual`. Guide page: `book/agents/net-vehicles.md`.

### 9.2 Hover hulls (added by ADR-8819)

A hover hull re-runs per entity like a character: its step is `det_hover::tick_hull`, a pure function of the hull tuning, its `DetHoverState`, its input, the static floor sampler and `dt`. `regia_framework::net_hover` adds `net_hover_step::<I>` to the predicted schedule: the server steps every `NetHover` hull, the owner steps its `Predicted` copy, and a rollback writes `DetHoverState` (full `f32` precision on the wire) and `Transform` and re-runs. `DetHoverPlugin` skips these hulls (`DetHoverSteppedExternally`), so proxies keep their interpolated pose. Default command `HoverNetInput { throttle, strafe, steer (8 bits each), hop, jump }`; `on_missing` clears `hop` and `jump`. One entity tolerance over `DetHoverState`: position and hover height 0.01 m, angles 0.01 rad, velocities 0.05 m/s, rates 0.05/s, `hop_cooldown` 1/240 s, flags exact; `Transform` is derived and not compared. The hull's tuning is named by `NetHover::hull` (a key over the hull's 273 handshake bytes and footprint) and looked up in `NetHoverHulls` on each side; an unknown key does not step. On the server an outside move of the hull's `Transform` (knockback, `net.force_correction`) is taken into its state at the next step. The step has no side effects, so `NetResimulating` suppresses nothing in it; hop and jump launches reach `NetHoverEffects` only on a first run (§ 8). Agent method `net.hovers`; guide page `book/agents/net-hover.md`.

## 10. Files and formats

- `Predicted` and `TickDrivenMotion` are reflected components in prefabs/levels: additive, no version bump; after the v0.5.0 freeze, shape
  changes need a component version and a migrator.
- `config/net.ron` section `prediction`:

```ron
prediction: (
    input_redundancy: 16,
    history_ticks: 64,
    max_resim_ticks: 32,
    margin_target_ticks: 2,
    margin_max_ticks: 8,
    dilation_window_ticks: 8,
    max_dilation: 0.05,
    smooth_half_life_ms: 40.0,
    snap_distance_m: 2.0,
    spawn_grace_ticks: 8,
    max_input_packets_per_tick: 4,
    trust_client_within_m: None,
),
```

- The input type's codec is part of the protocol hash (spec 70 § 6.1).
- Format page `docs/formats/net-ron.md` (created with spec 70) lists the `prediction` keys; every key has a serde default.

## 11. Determinism

- Prediction relies on the predicted schedule being deterministic for the same build: same inputs, same starting state → same result. Debug
  builds can verify it: `prediction.verify_resim: true` re-runs every tick twice from the same start and compares (cost ×2; tests only).
- Different builds or platforms may differ in the last float bits; mismatches are then corrections, which the tolerances and smoothing absorb.
  The acceptance tests check zero corrections only within one platform; the cross-platform loopback test (Linux server, Android client on
  the lead's device run) records the correction rate instead.
- The engine's own predicted pose math is bit-stable across platforms by construction: `Tolerance` pose/angle comparisons,
  `TickDrivenMotion` interpolation and the `NetVisualOffset` correction smoothing use fixed-order IEEE scalar ops (ADR-4770), so a
  same-build server and client reconcile engine-owned poses bit-identically. Game-authored predicted systems are not covered by this and
  fall under the previous bullet.
- Resimulation never reads wall-clock time, frame `dt`, or non-predicted mutable resources.

## 12. Agent protocol surface

Guide page: `book/agents/net-prediction.md` (topic `net-prediction`): the four-step loop, what may run in the predicted schedule, first-time
gating, predicted spawns, moving bases, the tolerance trade-off, reading the stats, and every error code.

| Method | Mode | Params | Returns |
|---|---|---|---|
| `net.prediction` | read (client) | — | `{ client_tick, lead, rtt_ms, jitter_ms, margin, corrections_per_s, max_error_m, resim_ticks_last, resets, rejected_spawns, corrections, resyncs, teleports, bound_spawns, deferred, resim_depth, snaps, last_correction_m }` |
| `net.input_buffer` | read (server) | `{ peer }` | per player `{ newest_tick, margin, missing, late, early }` |
| `net.prediction_history` | read (client) | `{ entity, ticks? }` | per tick the predicted component values and whether the server confirmed them |
| `net.force_correction` | play (server, dev builds) | `{ entity, offset: [x,y,z] }` | `{ applied_tick }`: moves the server copy so the client must correct |

Errors (code `F0002` with `data.reason`, as in the neighboring built modules): `net_not_client`, `net_not_server`, `entity_not_predicted`, `peer_unknown`, `net_not_running`.

## 13. Tests and acceptance

Loopback hub with a conditioner in ticks (spec 70 § 10). "Latency 6 ticks" = 100 ms at 60 Hz.

1. **No false corrections.** A character walks a scripted 600-tick input recording at 6-tick latency, 2 ticks jitter, 5 % loss: zero
   corrections, server and client end at the same bit-identical position.
2. **One correction.** `net.force_correction` moves the server copy by 1 m at tick 300: exactly one correction, the client's simulated pose
   equals the server's from the next state on, the rendered offset decays below 1 mm within 400 ms of simulated time.
3. **Snap.** A 3 m forced error snaps (no smoothing); a teleport snaps and clears history.
4. **Redundancy.** 15 consecutive lost input packets lose no command (server `input_missing == 0`); 20 consecutive lost packets while the input
   changes during the gap produce repeats and one correction (with a constant input the repeats are right and no correction happens).
5. **Missing input.** With `on_missing` clearing `jump`, a held-forward input is repeated and a jump is not.
6. **Clock.** Starting with a lead 20 ticks too large, the client resyncs once; starting 3 ticks off, it converges within 120 ticks by
   dilation alone (no resync); the server's `input_early` and `input_late` stay 0 after convergence.
7. **No speed abuse.** A client that sends two commands per tick (a modified client in the test) moves no faster than one that sends one;
   the excess are dropped as early.
8. **Rollback set.** Two predicted entities of one player (character and a carried shield): a mismatch on one rolls back both.
9. **Resim cap.** With a 40-tick lead and a mismatch, the client resets instead of re-running 40 ticks.
10. **Moving base.** A character riding a tick-driven elevator for 600 ticks at 6-tick latency has zero corrections; on a physics-driven raft,
    corrections are smoothed and the final position matches the server.
11. **Root motion.** A montage started from predicted code at tick T plays with identical root motion on server and client; resimulation
    across its start reproduces it.
12. **Predicted spawn.** A predicted projectile binds to the server's spawn (one entity, no duplicate); a projectile the server refuses (the
    test's server denies the shot) despawns after the grace period with `NetPredictedSpawnRejected`.
13. **First time.** A sound triggered by predicted code plays once per tick even with three resimulations across that tick.
14. **Tolerance mode.** With `trust_client_within_m: Some(0.05)` a 2 cm offset is adopted (no correction) and a 10 cm offset is corrected.
15. **Character refactor.** All existing `regia_framework` character tests pass unchanged with `step_character` in place.
16. **Agent.** Every method round-trips; each error code is produced by a test.

## 14. Platform notes and performance budget

- Same code on Linux, Windows, Android aarch64, macOS (testing). The Android client is the cost case: resimulation runs the predicted
  schedule several times in one frame.
- Client, Android reference class (`06-testing-and-ci.md` § 3; measured on the owner's Pixel 6): re-running 8 ticks for one character with rapier queries under 1.0 ms (span `net_resim`); the common no-correction
  path (compare only) under 0.05 ms.
- Server (Desktop reference class): input buffer handling for 64 players under 0.1 ms per tick (span `net_input`).
- Memory per predicted entity: `history_ticks` × the size of its predicted components (a character: about 64 × 96 bytes).

## 15. Out of scope

Predicting proxies (spec 74 extrapolates them instead); rolling back the whole world; client-side hit decisions (the server decides with
spec 75); deterministic lockstep for RTS-style games; prediction of physics-simulated rigid bodies other than characters (a later spec may
add predicted rigid bodies; until then they are proxies).

## 16. Provenance

- Set spec id used: `runtime-networking-movement-prediction` (behavior only, ADR-1526).
- Public sources: G. Gambetta, "Fast-Paced Multiplayer" article series (client-side prediction, server reconciliation with an input sequence
  number, entity interpolation); T. Ford, "Overwatch Gameplay Architecture and Netcode", GDC 2017 (input redundancy, the client running ahead
  with time dilation to keep the server's input buffer filled, rollback and resimulation of predicted state); G. Fiedler, "Gaffer on Games":
  "Networked Physics" and "Fix Your Timestep!"; Valve Developer Community wiki, multiplayer networking article (user commands, prediction,
  the server running the same movement code).
- Regia design choices: one command per tick consumed on the server clock instead of client time steps, the margin feedback and its
  defaults, all-predicted-entities rollback, the visual-offset half-life smoothing, tick-driven movers, predicted spawn keys and their
  grace period, the optional position tolerance.

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

## 17. Owner decisions

None. Review notes (ADR-1585): the design (one command per tick on the server clock, margin feedback, all-predicted-entity rollback,
visual-offset smoothing, predicted spawn keys) is general and shares no names with the set's movement-component description; the
set was used only to find edge cases (§ intro). `step_character` gains a collision-backend parameter because the built system removes
that resource while stepping. The host-visible `MoveResult.ground_entity` field is listed under Upgrade in the builder's release note.
