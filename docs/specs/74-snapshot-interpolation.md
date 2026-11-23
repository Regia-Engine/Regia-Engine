# 74 — Snapshot interpolation for proxies

Status: proposed (ADR-1599). Plan item D3 (`master-plan.md` § 12: "interpolation"), rung v0.5.0. Crate: `regia_net::interp`.
Builds on specs 71 (replicated state, teleport bit), 72 (state packets with server ticks, profiles) and 73 (`NetClock`, the input stream
that carries the client's view tick to the server for spec 75).

Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: `regia_net` does not exist in the tree yet (no `crates/regia_net`); this spec is entirely new. Existing pieces it uses:
`FixedTime::overstep_fraction` (render interpolation of authority bodies, `regia_physics::sync`), `Visibility` and `Transform`
(`regia_scene::components`), `step_character` and `CharacterState` (`regia_framework::character`, refactored by spec 73 § 9).

Builders read this file plus specs 70-73 (netcode batch, ADR-1599) and the ambient docs. Facts used from those specs: spec 70 § 5.3 gives
each entity a role (authority, predicted, proxy); spec 71 gives replicated components with a codec (`ReplicateCodec`), the transform fast
path (position, rotation, optional linear velocity, a teleport bit), `NetChanged<C>` notices and `NetDespawning`; spec 72 gives state
packets stamped with the server tick of the state they hold, `replication.send_every_ticks` (default 2 at 60 Hz) and `.netprofile.ron`;
spec 73 defines the `NetClock` resource whose `server_tick_estimate` and `interp_tick` this spec maintains, and the input command that
carries the view tick (spec 73 § 3, § 4). All network types live in `regia_net` (layer 3).

Source area (public-sources spec set, ADR-1526; behavior only): `runtime-networking/runtime-networking-movement-prediction`
(the parts on how remote characters are shown: smoothing toward replicated data, snap distance, teleports, adopting the server's
movement mode), `runtime-networking/runtime-networking-overview`. Re-derived from public sources (§ 14); no wording, identifiers or
settings carried over.

## 1. Purpose and scope

Proxies (spec 70 § 5.3) only change when state arrives, at the send rate (default 30 per second), with jitter and loss. Shown as they
arrive they stutter. Instead, each client shows proxies a short, adaptive delay in the past, interpolating between two received states
that bracket the shown time. When data runs out, it extrapolates briefly, then holds.

In scope: the per-entity snapshot buffer, the client's interpolation timeline and its adaptive delay, interpolation of transforms and of
registered components, step (non-interpolated) components, extrapolation and recovery, snaps and teleports, spawn and despawn timing,
the view tick reported to the server, and proxy animation inputs.

Not in scope: predicted entities (spec 73), authority entities (rendered with the engine's existing fixed-step render interpolation,
`FixedTime::overstep_fraction`), lag compensation itself (spec 75).

## 2. Terms

| Term | Meaning |
|---|---|
| Interpolation tick | The fractional server tick at which this client shows proxies: `interp_tick = server_tick_estimate − delay_ticks`. Exposed in `NetClock`. |
| Delay | How far in the past proxies are shown, in ticks (fractional). Adapts to arrival jitter (§ 4). |
| Snapshot buffer | Per proxy entity, the received states of its interpolated components, keyed by server tick (ring of 32). |
| Bracketing pair | The newest buffered state at or before the interpolation tick and the oldest after it. |
| Starved | No buffered state after the interpolation tick: the entity extrapolates, then holds. |

## 3. What is interpolated

```rust
impl InterpolateAppExt for App {
    /// `C` must be replicated (spec 71). `lerp(a, b, t)` with `t` in [0, 1].
    fn interpolate<C: Component + ReplicateCodec + Clone>(&mut self, lerp: fn(&C, &C, f32) -> C) -> &mut Self;
}
```

- `Transform` (from spec 71's transform fast path) is always interpolated: position linear or Hermite (§ 5), rotation normalized
  quaternion lerp along the shorter arc, scale linear.
- Components registered with `interpolate` are blended with their function each frame.
- All other replicated components on a proxy are **step** components: they take a received value when the interpolation tick reaches the
  value's tick, not when the packet arrives. This keeps a proxy's state (its movement mode, crouch flag, weapon id) in step with its
  pose. `NetChanged<C>` notices (spec 71 § 7) for step components fire at that moment.
- Per-profile choice (`.netprofile.ron`, spec 72 § 8, field `interpolation`): `Linear` (default), `Hermite` (uses replicated velocity,
  for fast curved motion), or `None` (apply the newest state as it arrives; for entities whose motion does not matter visually).

## 4. Timeline and adaptive delay

- The client tracks each state packet's arrival time against the server tick it carries, giving a running estimate of the server tick at
  any local instant (`server_tick_estimate`) and of the arrival jitter (mean absolute deviation over the last 64 packets).
- Target delay = `replication.send_every_ticks × interpolation.delay_send_intervals` (default 2.0) + `jitter_ticks × 2`, clamped to
  `[interpolation.min_delay_ms, interpolation.max_delay_ms]` (defaults 33 ms and 250 ms).
- The delay moves toward its target by changing the speed of the interpolation clock, never by jumping: up to 10 % faster or slower than
  real time when the target is above the current delay (late data costs visible hitches, so it reacts within about half a second), and
  at most 2 % when lowering the delay (so a short calm period does not make the next burst of jitter starve).
- If the delay error exceeds 500 ms (a long stall, a resumed app on Android), the clock resyncs and logs `net.interp_resync`: if the delay
  must shrink, `interp_tick` jumps forward to the target; if it must grow, `interp_tick` holds (stops) until the target catches up with it.
- `interp_tick` is monotonic: it never goes backward, also across a resync.

## 5. Per-entity interpolation

Each frame, for each proxy with a snapshot buffer:

1. Find the bracketing pair `(a, b)` around `interp_tick`. `t = (interp_tick − a.tick) / (b.tick − a.tick)`.
2. **Snap check.** If `b` carries the teleport bit, or the distance between `a` and `b` positions exceeds the profile's `snap_distance_m`
   (default 5.0 m), do not blend across the pair: show `a` until `interp_tick` reaches `b.tick`, then `b`.
3. **Blend.** `Linear`: lerp positions. `Hermite`: cubic Hermite with the replicated velocities of `a` and `b` scaled by the tick span
   (falls back to linear if either velocity is missing). Rotation and registered components as in § 3.
4. **Step components** whose tick is at or before `interp_tick` are applied.
5. States older than the newest one at or before `interp_tick` are dropped, keeping one as the left bracket.

**Gap rule.** The client cannot tell an entity that did not change from one that changed but was not sent yet (budget, priority or
`min_interval_ticks`, spec 72 § 6). So when the two states of a pair are more than `2 × send_every_ticks` apart, the pair is not blended
over its whole span: the left state is held until `b.tick − send_every_ticks`, and the blend runs over the last send interval only. A
stopped entity's last state carries zero velocity, so holding it is exact; a starved moving one catches up over one interval.
Refined (ADR-1662 (9b)): the hold applies only when the left state is at rest (zero or no replicated velocity). A left state with a
velocity belongs to a moving entity whose states were lost and that was shown extrapolating; it blends over the whole span, so the
shown pose stays continuous.

## 6. Spawn, despawn and out-of-scope timing

- A proxy spawned from a spawn record with tick S is placed at its spawn state and kept hidden (`Visibility::Hidden` and no collider)
  until `interp_tick ≥ S`, so a projectile does not appear ahead of the shooter's muzzle as shown. Profile `appear_immediately: true` turns
  this off.
- Messages that target proxies run on arrival unless their type sets `ALIGN_TO_VIEW` (spec 70 § 6.5 item 3), in which case they are held
  until `interp_tick` reaches their tick, like step components.
- A despawn with tick D for an interpolated proxy emits `NetDespawning` on receipt (spec 71 § 4.2) and marks the entity `NetPendingDespawn`;
  the entity is despawned when `interp_tick ≥ D`, so the last moments (a car's final slide) are shown. Gameplay queries on clients should
  exclude `NetPendingDespawn` (the guide page says so). Profiles with `interpolation: None` despawn on receipt.
- Out-of-scope level-bound entities (spec 71 § 4.5) stop interpolating and hold their last pose.

## 7. Starvation, extrapolation and recovery

- No state after `interp_tick`:
  - If the newest state's replicated velocity is zero (or the entity has no velocity), hold it: an entity that stopped is not "starved",
    and the server sends nothing for unchanged entities (spec 72 § 6).
  - Otherwise extrapolate from the newest state with its velocity, up to `max_extrapolate_ms` past it (profile, default 100 ms; 0
    disables). Beyond that, hold and insert `NetStarved` (removed when data resumes).
- When a new state arrives after extrapolating, the shown pose does not jump: the difference between the extrapolated pose and the newly
  interpolated pose becomes a visual offset that decays with half-life `interpolation.recover_half_life_ms` (default 60 ms), as in spec 73
  § 6.4. Differences above `snap_distance_m` snap. The recovery offset is position only; rotation follows the interpolated pose
  (ADR-1662 (9e)).
- Counters: frames starved, frames extrapolated, recoveries, snaps.

## 8. The view tick (for lag compensation)

- Every input command (spec 73 § 3) carries the client's `interp_tick` at the moment the command's tick was sampled, quantized to 1/256
  tick, plus the delay. The server stores it with the command; when it consumes the command for tick T (spec 73 § 5.2) it sets the player's
  `NetViewTick` (server-side component on the `Player` entity) to that command's view tick. A repeated command (missing input) keeps the
  previous value. Commands buffered for later ticks never change it early. Spec 75 rewinds hit checks to it.
- A client whose reported view tick is older than `lag_compensation.max_rewind_ms` (spec 75) or newer than the server tick is clamped by the
  server; spec 75 owns those rules.

## 9. Proxy animation and movement modes

- Animation graphs on proxies read interpolated values (speed from the interpolated velocity, the movement mode from the step component),
  so foot placement follows the shown pose, not the newest packet.
- A proxy character's movement mode is the server's (step component); proxies never run `step_character`. Their rapier bodies are
  kinematic (spec 71 § 9) and follow the shown pose, so client-side traces against proxies hit what the player sees.

## 10. Files and formats

- `.netprofile.ron` (spec 72 § 8, still `format_version: 1`, written together with spec 72) gains the fields this spec owns:
  `interpolation: Linear | Hermite | None` (default `Linear`), `snap_distance_m: 5.0`, `max_extrapolate_ms: 100`,
  `appear_immediately: false`. They have serde defaults, so profiles written without them load.
- `config/net.ron` section `interpolation`:

```ron
interpolation: (
    delay_send_intervals: 2.0,
    min_delay_ms: 33.0,
    max_delay_ms: 250.0,
    raise_rate: 0.10,
    lower_rate: 0.02,
    resync_error_ms: 500.0,
    recover_half_life_ms: 60.0,
    buffer_states: 32,
),
```

- Migration note: both files are unfrozen until v0.5.0; additions until then come with serde defaults; at the freeze they are registered in
  `check-formats` and `upgrade-project`.

## 11. Determinism

- Interpolation is presentation. It never feeds the server and never changes replicated state. Given the same packet arrival log (ticks and
  local arrival times) it produces the same shown poses, which is how the tests run: arrival times come from the loopback hub's simulated
  clock, never the wall clock.
- On a real network the shown poses depend on arrival timing by design; replays (the existing replay system) record the replicated state, not
  the shown poses.

## 12. Agent protocol surface

Guide page: `book/agents/net-interpolation.md` (topic `net-interpolation`; add its `book/COVERAGE.md` row): the delay and why proxies are behind, step versus interpolated
components, spawn and despawn timing, starvation states, the profile fields, how to read the stats, and every error code.

| Method | Mode | Params | Returns |
|---|---|---|---|
| `net.interpolation` | read (client) | — | `{ interp_tick, delay_ms, target_delay_ms, jitter_ms, clock_rate, starved, extrapolating, snaps_per_s, resyncs }` |
| `net.proxy_timeline` | read (client) | `{ entity }` | `{ buffered: [tick], bracket: [a, b], t, state: interpolating\|extrapolating\|starved\|holding }` |
| `net.set_interp_delay` | play (client, dev builds) | `{ ms \| null }` | `{ delay_ms }` (null returns to adaptive) |

Errors: `net_not_client`, `entity_not_proxy`, `delay_out_of_range` (outside 0..1,000 ms), `net_not_running`.

## 13. Tests and acceptance

Loopback hub with simulated arrival times and a seeded conditioner.

1. **Smooth constant motion.** An entity moving at constant velocity, sent at 30 Hz with no jitter: the shown position on every client frame
   (at 60, 90 and 144 Hz frame rates) equals the analytic position at `interp_tick` within 1 mm.
2. **Jitter.** With ±30 ms arrival jitter the adaptive delay settles within 2 s, and over 60 s no frame is starved.
3. **Delay adaptation.** A jump in jitter raises the delay within 0.5 s; a calm period lowers it at no more than 2 % clock change; a 2 s stall
   triggers one resync and `interp_tick` stays monotonic.
4. **Loss.** With three consecutive lost states (one lost state is covered by the two-interval delay and never starves) the entity extrapolates and recovers with an offset that decays (no jump larger than 1 cm per
   frame).
   **Gap.** An entity that rests for 2 s and then moves (its first new state 2 s after the last) stays at rest until one send interval
   before the new state, then blends over one interval; it never drifts during the rest.
5. **Starvation.** With the stream cut, the entity extrapolates for 100 ms, then holds with `NetStarved`; a stopped entity with no updates is
   held without `NetStarved`.
6. **Snap and teleport.** A teleport bit produces no in-between frames; a 6 m jump between states snaps; a 4 m one blends.
7. **Hermite.** A projectile on a ballistic arc shown with `Hermite` stays within 2 cm of the analytic arc at 30 Hz send rate; with `Linear` the
   error is larger (asserted, documenting why the option exists).
8. **Step components.** A crouch flag received early applies exactly when `interp_tick` reaches its tick.
9. **Spawn and despawn timing.** A spawned proxy stays hidden until its spawn tick is shown; a despawned one stays until its despawn tick is
   shown and carries `NetPendingDespawn` meanwhile; `appear_immediately` and `interpolation: None` bypass both.
10. **View tick.** The server's `NetViewTick` for a player equals the client's reported `interp_tick` within 1/256 tick, for every command.
11. **Agent.** Every method round-trips; each error code is produced by a test.

## 14. Platform notes and performance budget

- Same code on Linux, Windows, Android aarch64 and macOS (testing). On Android, `resumed` after a pause triggers the § 4 resync (the app was
  stopped, so the clock error is large); the surface lifecycle is unaffected (interpolation is CPU-only). The app's real-time clock
  restarts without a frame gap on `resumed`, so the trigger is the first states after the pause, which arrive far ahead of the arrival
  estimate: the estimate restarts from them and the clock jumps forward once (ADR-5649). `NetInterpolation::resync()` forces a resync.
- Client budget: 500 interpolated proxies under 0.3 ms per frame on the Android reference class (`06-testing-and-ci.md` § 3; measured on the owner's Pixel 6) (span `net_interp`); buffer memory 32 states per
  proxy (transform about 40 bytes plus registered components).

## 15. Out of scope

Decision (ADR-1585): the profile fields of § 10 are serde-defaulted additions to a v1 format that is unfrozen until v0.5.0, so no migrator
is needed before the freeze; after it, any shape change needs a version bump, a migrator and `regia upgrade-project` support.

Interpolating authority entities across fixed steps (existing render interpolation); spline paths through more than two states; per-bone
replication of animation poses (proxies animate locally from parameters); client-side prediction of proxies beyond the extrapolation limit.

## 16. Provenance

- Set spec ids compared in review (ADR-1585): `runtime-networking-movement-prediction`, `runtime-networking-overview`. The set describes a
  distance-bounded blend of a mesh offset; this spec uses a snapshot buffer with an adaptive delay and a decaying recovery offset, so
  structure, names and defaults differ. Set spec ids used: `runtime-networking-movement-prediction` (remote-character smoothing parts), `runtime-networking-overview` (behavior
  only, ADR-1526).
- Public sources: G. Fiedler, "Gaffer on Games": "Snapshot Interpolation" (interpolation buffer and delay, Hermite interpolation with
  velocities); G. Gambetta, "Fast-Paced Multiplayer: Entity Interpolation"; Valve Developer Community wiki, multiplayer networking article
  (interpolation delay of about two snapshot intervals, extrapolation limits); Y. Bernier, "Latency Compensating Methods in Client/Server
  In-game Protocol Design and Optimization", GDC 2001 (reporting the client's view time to the server).
- Regia design choices: the asymmetric clock-rate adaptation and its rates, the hold-versus-starved rule for unchanged entities, delayed
  spawn and despawn by default, the recovery offset half-life, the view tick quantization.

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
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
