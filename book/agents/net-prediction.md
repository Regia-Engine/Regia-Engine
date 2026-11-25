# Net prediction (agents)

<!-- index: Prediction -->
<!-- index: Reconciliation | see: Prediction -->
<!-- index: Rollback -->

Client prediction and server reconciliation in `regia_net::predict` (spec 73, ADR-1793). Guide topic `net-prediction`.
When to use prediction instead of lockstep is [Lockstep and rollback](../how-to/online/lockstep-and-rollback.md).
With a 100 ms round trip, a client that waits for the server before moving its own character feels broken. Prediction
lets the owning client simulate its own entities at once from its own input, while the server stays the authority.

## The four-step loop

1. Each fixed step the client stamps its input with its **client tick** and sends it, redundantly, to the server.
2. The client runs the same gameplay step locally for that tick and remembers the result (the **prediction history**).
3. The server runs the step for that tick with the received input and sends back its state for the tick.
4. The client compares. When they differ it rewinds every predicted entity to the server's state for that tick and
   re-runs the ticks since with the inputs it remembered (**reconciliation**). The rendered pose blends out the jump.

Offline and on a listen-server host the local command is consumed directly: no stream, no lead, no rollback. Proxies
(other players' entities) are not predicted; they are interpolated (spec 74, [net interpolation](net-interpolation.md)).

## Setting it up

- One input type per game: `regia_net::add_net_input::<I>(world)` (or `app.add_net_input::<I>()`). `I: NetInput` is a
  `ReplicateCodec` value with `Clone + Default + PartialEq + Debug`; `NetInput::on_missing(last)` is what the server uses
  when a tick's command is missing (the default repeats it; clear one-shot fields like `jump` there). A second call is
  refused (`net.input_already_registered`). The input codec is part of the protocol hash.
- The game writes `InputCommand<I>.value` on its controller entity (an entity with `NetPlayer`) in `FixedUpdate`
  before `NetSet::Predict`; `regia_net` stamps `tick`. On a client it creates the controller for local player 0 when
  none exists. On the server `regia_net` writes the consumed command into the same component, so step systems read
  input the same way on both sides.
- Mark the entities the owner predicts with `Predicted` (the same component as `NetPredicted`; saved in prefabs). An
  owned entity without it is a proxy on its owner too.
- Register the predicted components with a tolerance: `regia_net::predict::<C>(world, Tolerance::...)`. `C` must be
  replicated (spec 71); `Transform` counts through the fast path. `Tolerance::Exact` compares the wire bits;
  `Tolerance::position(m)`, `angle(rad)`, `pose(m, rad)` for transforms; `Tolerance::Fn` for your own rule.
- Add the step systems with `add_predicted_systems(world, systems)` (`app.add_predicted_systems`). They run in the
  `PredictedUpdate` schedule, from `NetSet::Predict`: on the server for every entity, on the owning client for
  `NetRole::Predicted` copies, once per tick and again during resimulation.

## What may run in the predicted schedule

Predicted systems read only predicted components, `InputCommand<I>`, `NetClock`, read-only world geometry, and the
poses of proxies (as shown this frame; proxies are never rolled back, so pushing another player can mispredict and is
corrected by the server). They must not spawn non-predicted replicated entities, send net messages directly, or touch
other resources except through `PredictionCx`. Debug builds warn `net.predict_writes_unregistered` once per component
type the schedule wrote on a predicted entity without registering it. Nothing in the schedule may read wall-clock
time or frame `dt`: the step is the fixed `dt`.

`PredictionCx` (system parameter):

| Call | Meaning |
|---|---|
| `tick()` | the tick being simulated |
| `is_resimulating()` | re-running a tick after a correction |
| `first_time()` | true while running a tick for the first time on this client; false during resimulation and on the server |
| `simulates(role)` | whether this process steps an entity with this role (every entity on the server, `Predicted` copies on a client) |
| `owner_player(owner)` | the player whose input drives an entity (`NetOwner` on the server, local player 0 on a client) |
| `spawn_predicted(player, bundle)` | spawn ahead of the server, returns `(Entity, PredictionKey)` |
| `send_local_event(e)` | write a local message, dropped while resimulating |

## First-time effects

Sounds, muzzle flashes and camera shakes triggered by predicted code are gated on `first_time()` (or sent with
`send_local_event`), so three resimulations across a tick still play them once. On the server predicted systems run
once per tick; effects for other clients go through replicated state or broadcasts with `SKIP_PREDICTING_OWNER`.

## The input stream and the lead

- Every client tick one unreliable `INPUT` frame carries, per local player, every command the server has not
  acknowledged, newest first, up to `input_redundancy` (16); repeated values cost one bit each.
- The server keeps commands keyed by tick up to 64 ticks ahead and consumes exactly one per player per tick on its own
  clock. Missing: `on_missing(last)` (counted `missing`). Late (for a tick already simulated with a repeat): merged into
  the next consumed tick with `NetInput::merge_late` (spec 120 § 6.3: presses kept, newest analog values; a duplicate
  merges nothing), counted `late`. More than 64 ahead: dropped, counted `early`. The packet limit is a token bucket:
  `max_input_packets_per_tick` (4) tokens per server tick, at most `INPUT_BURST_TICKS` (16) ticks' worth (64), so a
  burst a stalled link flushes is kept and a peer that sends more than 4 every tick is cut to 4; dropped packets are
  counted `flood`. A client that sends commands faster than real time gains nothing.
- Every tick the server acknowledges per player the newest input tick, the **buffer margin** (how many ticks early the
  newest command arrived, averaged over 30 packets) and the ticks it consumed with repeats, so a resimulation uses what
  the server used.
- The client steers its lead so the margin sits on `margin_target_ticks` (2) plus one tick per 4 ms of jitter (at most
  `margin_max_ticks`, 8): a small error **dilates** the client's time (spec 120 § 9.3, ADR-8217): the continuous clock
  runs `1 -+ max_dilation` ticks per fixed step (±5 %; gameplay `dt` stays the step), and a step runs the ticks that start
  in it, so one tick in `1 / max_dilation` steps is skipped or doubled. Predicted entities are drawn at the continuous time
  (the dilation part of `NetVisualOffset`, between the last two predicted ticks; `dilation_lag` ticks behind the newest),
  so the pawn moves `1 -+ max_dilation` ticks' worth every frame, never 0 or 2. An error over `dilation_window_ticks`
  (8), or a negative margin 10 reports in a row, jumps the tick and logs `net.lead_resync`. Ticks jumped backward are not
  sent again; a press made while the stream holds stays latched for the first tick that is sent.
- **Input capture** (spec 120 § 9.3): the game writes `InputCommand` once per fixed step; the step latches it (a step
  that runs no tick keeps the latch and the next step merges into it with `merge_late`), the first tick of the step takes
  it and a second tick gets `on_missing` of it, so a one-step press reaches exactly one tick on the client and on the
  server. After the step `InputCommand` holds the value the last tick used. Override `NetInput::merge_late` field by
  field for an input with presses (OR or add them, take the newest analog values); the default keeps a press only one of
  the two carries.

## Rollback

On the server state for tick S: all predicted values within tolerance of the history at S drop the history at or
below S (the common case). Any mismatch writes the server's S values into **every** predicted entity (they stay
consistent with each other) and re-runs S+1 ..= client tick with the remembered inputs (`NetResimulating(true)` while
it runs; collision backends skip push impulses then). No history at S because S left the ring (a lead resync): the
same rollback, counted `resets`. A predicted entity new to the client (spawned or made predicted after the clock
started) takes the same rollback to catch up, counted nowhere (debug event `net.prediction_adopted`).

The rollback depth scales with the round trip (spec 120 § 9.4, ADR-8219): `resim_depth` = RTT in ticks + margin target
+ 2 + one state send interval, at least `max_resim_ticks` (32) and at most `history_ticks` (64, up to 128). A rollback
deeper than that (an RTT spike) still writes S and re-runs to the client tick from the input history, counted `resets`:
prediction stays on at 450 and 650 ms RTT. Only a rollback deeper than the 128-tick input history clears the history and
snaps. Several states in one frame: only the newest S is used.

With a transport (`NetLink`), the clock waits for the transport's first round-trip sample before it starts (at most
120 fixed steps after the first server state), so the first lead is the measured round trip plus the margin target.

## Smoothing and teleports

The simulated pose jumps at once; `NetVisualOffset` keeps the difference and decays it with `smooth_half_life_ms`
(40 ms). Rendered pose = `offset.apply(simulated)`; children with `NetVisualChild { base }` get their local transform
shifted by it. An offset over `snap_distance_m` (2 m) or 90° snaps. A teleport bit (spec 71 § 9) on the server state
always snaps and is not counted as a correction. `net.prediction` counts every rollback whose rendered pose jumped in
`snaps` (a teleport, an offset over the snap distance, a cleared history) and reports the pose change of the last
rollback in `last_correction_m`; a rollback over `resim_depth` smooths like any correction.

## Moving bases

`TickDrivenMotion { keys, start_tick, mode }` (Once, Loop, PingPong; key times in seconds, linear position, nlerp
rotation) is a pure function of the tick, evaluated at the predicted tick on the client and the server tick on the
server, so a character riding it predicts exactly. Replicate such movers with `TransformReplication::None`. Use them for
anything players stand on. Characters on other bases (physics-driven boats) keep their place on the base, see
[net characters](net-character.md).

## Predicted spawns

`spawn_predicted(player, bundle)` spawns a local entity with `NetPredictionKey { player, tick, index }` (index counts
that player's predicted spawns in that tick, in system order). The server, running the same code for the same tick,
spawns the real replicated entity with the same key. Give the bundle `NetPredicted` and `NetOwner` so the bound copy
stays predicted. The client binds the server's spawn record to its local entity (one entity, its handle stays valid;
`PredictionEvents.bound`) and reconciles it like any predicted entity: a right prediction keeps its own pose for the
client tick and counts no correction; a wrong one rolls back and is smoothed (ADR-5649). If the server's spawn has not arrived when the acknowledged input tick reaches
`key.tick + spawn_grace_ticks` (8), the local entity is despawned and `NetPredictedSpawnRejected { entity, key }` is in
`PredictionEvents.rejected` (play a fizzle). During resimulation an existing key returns the existing entity.

## Client position tolerance (opt-in)

`prediction.trust_client_within_m: Some(m)`: each command also carries the client's predicted position (of its
lowest-`NetId` predicted entity with a transform). When the server's result is within `m`, the client's position
becomes authoritative for that tick (velocity stays the server's). This hides tiny cross-platform floating-point drift
**at the cost of letting a cheater move up to `m` metres per second** (the adopted offsets draw on a per-player budget
of `m` that refills at `m` metres per second, so a client cannot gain `m` every tick). Leave it `None` unless you
measured drift.

## Reading the stats

| Method | Mode | Params | Returns |
|---|---|---|---|
| `net.prediction` | read (client) | — | `{ client_tick, lead, rtt_ms, jitter_ms, margin, corrections_per_s, max_error_m, resim_ticks_last, resets, rejected_spawns, corrections, resyncs, teleports, bound_spawns, deferred, dilated, dilation_lag, resim_depth, snaps, last_correction_m }` |
| `net.input_buffer` | read (server) | `{ peer }` | `{ players: [{ local, newest_tick, margin, missing, late, early, flood, trusted, buffered }] }` |
| `net.prediction_history` | read (client) | `{ entity, ticks? }` | `{ ticks: [{ tick, values: { component: value }, confirmed }] }` |
| `net.force_correction` | play (server, dev builds) | `{ entity, offset: [x, y, z] }` | `{ applied_tick }`: moves the server copy so the client must correct |

A healthy client shows `corrections_per_s` near 0, `margin` near its target and `resyncs` that stop growing after the
start. Corrections that track another player mean interactions with proxies (expected); steady corrections alone mean
the predicted schedule is not deterministic (turn on `prediction.verify_resim` in a test to find it: it re-runs every
tick twice and logs `net.predict_nondeterministic`). A growing `missing` with a negative `margin` means input arrives
too late: the lead is short (check `rtt_ms`, `jitter_ms`). `dilated` sums the ticks dilation inserted (+) or skipped
(−); it drifts steadily when the client's clock runs fast or slow against the server's, which is expected.
`dilation_lag` (0..1 ticks) is how far behind its newest predicted tick the client draws its predicted entities; 0 when
the clock is on target.

```json
{"jsonrpc":"2.0","id":1,"method":"net.force_correction","params":{"entity":4294967310,"offset":[0,0,1]}}
{"jsonrpc":"2.0","id":2,"method":"net.prediction","params":{}}
```

## Errors

Errors are `F0002` with `data.reason`:

| Reason | When |
|---|---|
| `net_not_client` | `net.prediction` or `net.prediction_history` on a server or offline process |
| `net_not_server` | `net.input_buffer` or `net.force_correction` on a client |
| `net_dev_only` | `net.force_correction` in a shipping build (dev builds only, like `net.set_owner`; ADR-5993) |
| `entity_not_predicted` | the entity is not a `NetRole::Predicted` copy (client) or has no `NetPredicted` and `Transform` (server) |
| `peer_unknown` | no connected peer has that id |
| `net_not_running` | no input type registered, or the client has not received server state yet |

Log keys: `net.lead_resync`, `net.prediction_started`, `net.prediction_correction` (debug), `net.predicted_spawn_rejected`
(debug), `net.predict_writes_unregistered`, `net.predict_nondeterministic`, `net.predict_not_replicated`,
`net.input_already_registered`, `net.force_correction`.

## Per-entity reconcile ticks

On the state channel (spec 72 § 4) one server tick's state can come in several frames, and a profile's
`min_interval_ticks` or the bandwidth budget can leave an entity out of a send. A predicted entity that the newest
state tick did not carry still holds an older server state, so comparing it at that tick would count a false
correction. Since ADR-2413 each predicted entity is compared at its own tick: one the newest state tick did not carry,
while its last record is younger than a round trip plus two send intervals (the server must still have been sending
it), is left out of that tick's comparison and counted in `PredictionStats::deferred`; in a rollback caused by
another entity it moves back to its own prediction for that tick, not to its older server state. Past that window an
entity that is not sent is taken as quiet and compared as before, so a server that kept it still corrects a client
that moved it. `ClientPrediction::reconcile_tick(entity)` reads an entity's reconcile tick. On the reliable tick
stream every tick is complete and every entity reconciles at the newest tick.

## Not yet

Characters have their own plugin, which covers moving bases and montage root motion: [net characters](net-character.md).
