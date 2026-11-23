# Net interpolation (agents)

Snapshot interpolation of proxies in `regia_net::interp` (spec 74, ADR-1662). Guide topic `net-interpolation`.
Proxies (client copies this client neither owns nor predicts) change only when state arrives, at the send rate, with
jitter and loss. Shown as they arrive they stutter, so a client shows them a short, adaptive **delay in the past**:
every proxy buffers its received states by server tick, and each frame it is shown at the fractional
**interpolation tick** `interp_tick = server_tick_estimate − delay`, blended between the two states around it.

Proxies are therefore behind the server by design. A proxy that looks "late" by about two send intervals plus the
jitter is working as intended; predicted entities (spec 73) and the server's own entities are not delayed.

## Turning it on

- Interpolation is opt-in per client process: insert `NetInterpolation` (`install_interpolation(world, tick_hz,
  send_interval_ticks)`, or add `NetInterpolationPlugin` after `NetPlugin`). Without it, received transforms and
  components are written on arrival.
- The host stamps arrival times in `NetInterpolation.now_ms` before packets are applied and calls
  `net_interpolate(world, now_ms)` once per frame (`NetSet::Interpolate`, span `net_interp`). The plugin takes both
  from `RealTime`; tests pass a simulated clock, so the same arrival log always gives the same shown poses.
- After an Android pause the clock resyncs on its own: `RealTime` restarts without a frame gap on `resumed`, and the
  first states after the pause arrive far ahead of the arrival estimate, so the estimate restarts from them and
  `interp_tick` jumps forward once (`net.interp_resync`). A host clock that does see the pause (a frame gap longer
  than `resync_error_ms`) resyncs too, and `NetInterpolation::resync()` forces one.

## The delay

- The clock estimates the server tick at any local instant from state arrivals and tracks the arrival jitter (mean
  absolute deviation over the last 64 arrivals). An arrival later than the estimate by more than 3 x the larger of
  `2 × jitter` and one send interval is held out unless a second one follows within 8 arrivals (RFC 5905's popcorn
  spike suppressor, ADR-8312): one delayed state packet moves neither the estimate nor the delay, where it used to drag
  both for 64 arrivals. A later estimate offset is slewed in
  at 5 % of local time, so the estimate never steps backward.
- Target delay = `send_interval_ticks × delay_send_intervals` (default 2.0) + `2 × jitter` + the **loss term**,
  clamped to `[min_delay_ms, max_delay_ms]` (33 ms, 250 ms).
- The loss term (spec 120 § 9.7, ADR-8418) is how much the longest gap between two received state frames over the
  last 10 s of server ticks (`LOSS_MEMORY_MS`) exceeds the `delay_send_intervals` send intervals the delay already
  holds (at the default 2 they cover one lost frame): a run of three lost frames adds two send intervals, so the next
  frame after a burst is already there when the shown time needs it instead of the proxy extrapolating.
  A frame that arrives late (reordered) inside a gap splits it, so reordering is not counted as loss, and a gap
  counts only once frames two send intervals past it have arrived. A gap longer
  than `max_delay_ms` is an outage no allowed delay covers: it is not counted (extrapolation and resync handle it).
  `net.interpolation.loss_gap_ms` reads it.
- The delay moves toward its target by changing the clock speed, never by jumping: up to 10 % slower when the delay
  must grow (`raise_rate`), at most 2 % faster when it may shrink (`lower_rate`).
- A delay error above `resync_error_ms` (500 ms) resyncs and logs `net.interp_resync`: a delay that must shrink jumps
  forward; one that must grow holds the clock until the target catches up. `interp_tick` never goes backward.
- A burst of late packets arriving at one instant (after a stall) does not move the estimate; a clock shift that
  persists for at least half the resync error does.
- `net.set_interp_delay { ms }` fixes the delay (dev builds); `{ ms: null }` returns to adaptive.

## What is interpolated, what steps

- `Transform` is always interpolated: position linear (profile `interpolation: Linear`) or cubic Hermite with the
  replicated velocities (`Hermite`, falls back to linear when a state has no velocity); rotation normalized lerp
  along the shorter arc; scale linear. `NetVelocity` on the proxy is the interpolated velocity (animation input).
- Components registered with `app.interpolate::<C>(lerp)` (or `register_interpolated::<C>(world, lerp)`; `C` must be
  replicated) are blended with `lerp(a, b, t)` every frame. They emit no `NetChanged` notices under interpolation.
- Every other replicated component of a proxy is a **step** component: a received value is held (`NetStepQueue`) and
  applied when `interp_tick` reaches its tick, so a crouch flag or a movement mode changes with the pose, not with
  the packet. Its `NetChanged` notice fires at that moment (from `net_interpolate`, readable by `Update` systems).
  Removals of a component apply on receipt.
- `interpolation: None` in a proxy's profile skips all of this: the newest state is applied as it arrives.

## Remote characters' animation

A remote character's rig animates from its server's state, not from its own clock (ADR-2551). Add
`regia_framework::net_character::NetAnimState` to the replicated root (its rig is the root or the nearest
descendant with an `Animator`, playing the same graph on every process) and `NetAnimationPlugin` on every process.

- The server writes `NetAnimState` at the end of each fixed step (`net_anim_capture`): the animator's playback
  state as compact words (clip clocks, state-machine states and transitions; `regia_animation::pose_eval`) and the
  graph's declared parameters in declaration order. Declare every parameter the graph reads.
- `NetAnimState` is an interpolated component: float and vector parameters blend, ints, bools and triggers step,
  and `past` says how far the shown tick is past the state's tick.
- On the client, `net_anim_drive` runs before the animators evaluate. When the shown state's tick changes it
  re-anchors the proxy's animator on it (restores the playback state with all parameters, and adds `past` of a
  tick to the animator's next evaluation); in between the animator plays on with the blended parameters. Owned and predicted copies are left
  to their own game code.
- Measured on the demo character over the loopback hub (3 ticks of latency, walk, run, two jumps, an eased upper
  body layer): on every frame that re-anchors, every joint equals the server's own state of the shown tick evaluated at
  the shown time; on every frame it is within 3.4 cm of the linear blend of the server's two tick poses (the
  blend is a chord; the client shows the animation itself). The words are the whole playback state, layer weight
  easing included.
- A rig whose state the snapshot cannot hold (an inertialized transition, sync groups) is not replicated; the server
  logs `net.anim_state_refused` once and `net.anim_state.refused` is `true`.

## Snaps, gaps, starvation

- A state with the teleport bit, or a jump between two states longer than `snap_distance_m` (5 m), is not blended:
  the proxy shows the older state until the newer one's tick, then the newer one (counted as a snap).
- Gap rule: when two states of a resting proxy (zero velocity, or no velocity and the same position as the state
  before) are more than two send intervals apart, the older one is held until one send interval before the newer
  one, and the blend runs over that last interval only. A resting entity never drifts. (This stays on the send
  interval for a deferred proxy too: its next state arrives only about one delay before its tick, so a longer blend
  would hold and then jump.)
- Update interval (spec 120 § 9.7, ADR-8418): a proxy's own spacing between its last two states. A server bound by
  its bandwidth budget sends each entity only every few state frames (64 moving entities with room for 16 per frame:
  every 4th frame); the own interval then sets how long the proxy may be extrapolated before its next state is
  overdue (below). Spacings over `MAX_DEFERRAL_MS` (500 ms) are a rest, not a deferral.
  `net.proxy_timeline.update_interval_ticks` reads it.
- A proxy without `NetVelocity` whose last two states differ is **moving**: its velocity is inferred from them. A
  pair of its states blends over the whole span (no gap-rule hold). Past its newest state it extrapolates with the
  inferred velocity only while the budget defers it: state frames newer than its newest state have arrived without
  it, and its next state is not yet overdue (a frame newer than its newest state plus its update interval plus one
  send interval means it stopped); for at most its update interval or `max_extrapolate_ms`, whichever is longer.
  When the whole stream is late (no newer frame at all) it is held, as before, so a lossy link shows only states
  the server had (what lag compensation rewinds to). A constant-speed entity the budget defers is therefore never shown
  standing still between its updates. When it really stopped, the overshoot (at most about one update interval of
  motion) decays back to its last state like any recovery. The inferred velocity is not written as `NetVelocity`;
  replicate `NetVelocity` for exact stops.
- Starved (no state after `interp_tick`): a proxy whose newest state has zero velocity, or no velocity and no
  inferred motion (above), is **held** (it stopped; the server sends nothing for unchanged entities). A moving one **extrapolates** with its velocity for up to
  `max_extrapolate_ms` (100 ms; 0 disables), then is held at the limit and gets `NetStarved` (removed when data
  resumes).
- Recovery: when data resumes after extrapolating, the shown pose does not jump; the difference becomes a visual
  offset that halves every `recover_half_life_ms` (60 ms). An offset over `snap_distance_m` snaps.

## Spawn and despawn timing

- A new proxy spawned at tick S is placed at its spawn state with `Visibility::Hidden` and `NetHiddenUntilShown`
  until `interp_tick ≥ S` (then its previous visibility is restored), so a projectile does not appear ahead of the
  muzzle as shown. Profile `appear_immediately: true` turns this off.
- While hidden it has no collider either (ADR-2413): with `regia_framework`'s `net` feature
  (`NetSessionBridgePlugin`, added by `RulesPlugin`) the `Collider` of the proxy and its descendants waits in
  `NetHiddenCollider` and comes back when the proxy is shown, so nothing hits or is pushed by a proxy the player
  cannot see. It acts in observers the moment the marker or a collider is added, before any physics step.
- A despawn at tick D emits `NetDespawning` on receipt and inserts `NetPendingDespawn`; the entity is despawned when
  `interp_tick ≥ D`, so its last moments are shown. **Gameplay queries on clients should exclude
  `NetPendingDespawn`** (`Without<NetPendingDespawn>`). `interpolation: None` despawns on receipt.
- Out-of-scope level-bound proxies stop interpolating and hold their last pose.
- `ALIGN_TO_VIEW` messages wait for `NetInterpTick` (the integer part of `interp_tick`), which interpolation keeps up to
  date.

## The view tick

Each input command reports the client's `interp_tick` (1/256 tick, `ViewTickWire`) and its delay; the server sets
the player's `NetViewTick` from the command it consumes for a tick (`set_view_tick`), a repeated command keeps the
previous value. Spec 75 rewinds hit checks to it.

## Profile and settings fields

`.netprofile.ron` (spec 72 § 8, `format_version: 1`, serde defaults): `interpolation: Linear | Hermite | None`
(default `Linear`), `snap_distance_m: 5.0`, `max_extrapolate_ms: 100`, `appear_immediately: false`. On the client a
proxy's choice is the `NetInterpProfile` component; without it `NetInterpolation.default_profile`.

`config/net.ron` section `interpolation`: `delay_send_intervals: 2.0`, `min_delay_ms: 33.0`, `max_delay_ms: 250.0`,
`raise_rate: 0.10`, `lower_rate: 0.02`, `resync_error_ms: 500.0`, `recover_half_life_ms: 60.0`, `buffer_states: 32`.

## Methods

| Method | Mode | Params | Returns |
|---|---|---|---|
| `net.interpolation` | read (client) | — | `{ interp_tick, delay_ms, target_delay_ms, jitter_ms, loss_gap_ms, clock_rate, starved, extrapolating, snaps_per_s, resyncs }` |
| `net.proxy_timeline` | read (client) | `{ entity }` | `{ buffered: [tick], bracket: [a, b] \| null, t, state, update_interval_ticks }` |
| `net.set_interp_delay` | play (client, dev builds) | `{ ms \| null }` | `{ delay_ms }` (the delay the clock now steers toward) |
| `net.anim_state` | read (server or client) | `{ entity }` | `{ entity, role, tick, past, shown_tick, words, params, rig, rig_state, refused }` |

Reading the stats:

- `interp_tick` is fractional; `delay_ms` is the current distance to the server-tick estimate, `target_delay_ms`
  where it is heading, `clock_rate` the clock speed (1.0 = real time, 0.9 = growing the delay fastest, 0 = held
  after a resync).
- `starved` and `extrapolating` count proxies in that state on the last frame; `snaps_per_s` counts snaps over the
  last second; `resyncs` is the total.
- `loss_gap_ms` is the loss term inside `target_delay_ms`: 0 on a loss-free link or after single lost frames, about
  `(n − 1) × send interval` after a run of `n` lost state frames (default `delay_send_intervals`), back to 0 ten
  seconds after the last one.
- `net.proxy_timeline.update_interval_ticks` is the proxy's own update interval (ticks between its two newest states;
  `null` before it has two). Larger than the send interval means the server's budget defers it; such a proxy shows
  `extrapolating` between its updates when it has no `NetVelocity`.
- `net.anim_state.role` is `server`, `proxy` (the drive animates it) or `local` (owned or predicted);
  `shown_tick` is the state the proxy's animator was last re-anchored on; `params` decodes blending lanes as
  numbers and the others as raw lane values.
- `net.proxy_timeline.state` is `interpolating`, `extrapolating`, `starved` or `holding` (stopped, before its first
  state is shown, or `interpolation: None`). `bracket` is the pair of ticks around `interp_tick` (null when holding or
  extrapolating) and `t` the blend between them.

## Errors

Code `F0002` with `data.reason`:

| Reason | When |
|---|---|
| `net_not_client` | the process is a server or offline |
| `entity_not_proxy` | the entity is not an interpolated proxy (local, owned, predicted, or `interpolation: None`) |
| `delay_out_of_range` | `net.set_interp_delay` outside 0..1,000 ms |
| `net_dev_only` | `net.set_interp_delay` in a shipping build (dev builds only, like `net.set_owner`; ADR-5993) |
| `net_not_running` | the client has no `NetInterpolation`, or no state has arrived yet; for `net.anim_state`, the app has no `regia_net` runtime |
| `entity_not_found` | `net.anim_state`: the entity does not exist |
| `entity_not_anim_replicated` | `net.anim_state`: the entity carries no `NetAnimState` |

## Logs

- `net.interp_resync` (info): the clock resynced (fields `error_ms`, `frame_gap_ms`).
- `net.interpolate_not_replicated` (error): `interpolate::<C>` for a component that is not replicated.
