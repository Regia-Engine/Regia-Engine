# 75 — Lag compensation for hit checks

Status: proposed (ADR-1599). Plan item D4 (`master-plan.md` § 12: "lag-compensation history for hit checks"), rung v0.5.0. Crate:
`regia_net::lagcomp` (history and queries), using `regia_physics` for static-world occlusion and `regia_animation` for joint poses. Builds
on spec 74 § 8 (the view tick each client reports with its input).

Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built (ADR-1752, ADR-2551, ADR-5650): `regia_net::lagcomp`; the API as built, the differences from this text and the acceptance
status are in `90-lag-compensation.md`, which folds into this spec. Pre-build baseline: `regia_net` did not exist; this spec was new. Existing pieces used: `regia_core::math::Ray` (`bounds.rs`), the rapier
scene queries in `regia_physics`, `AnimationClip::sample_pose` (`regia_animation::clip`) for joint poses, `regia_core::id::Name`.

Builders read this file, specs 70-74 (netcode batch, ADR-1599), `15-physics.md` (scene queries) and `17-animation.md` (skeleton sampling).
Facts used from them: spec 70 defines `NetPlayerId` (peer + local slot), `NetSet` ordering (`NetSet::LagCompRecord` at the end of
`FixedUpdate`, § 8.1) and net messages (`#[derive(NetMessage)]` with direction, channel and rate limits); spec 71 gives `NetId`, `NetGroups`
and the teleport mark; spec 74 § 8 sets `NetViewTick` on the shooter's `Player` entity from the input command being consumed.

Source area (public-sources spec set, ADR-1526; behavior only): `runtime-networking/runtime-networking-movement-prediction` and
`runtime-networking/runtime-networking-overview` give the client-server timing context; the set has no dedicated lag-compensation spec, so
the design comes from public sources (§ 15).

## 1. Purpose and scope

A client shows other players about 100 ms in the past (spec 74) and its shot reaches the server about 50 ms later. If the server tests
the shot against where targets are *now*, a player who aimed perfectly at what they saw misses. Lag compensation keeps a short history of
where every hittable entity's hit shapes were on each server tick, and tests a client's shot against the shapes **as that client saw
them**, at the view tick it reported.

In scope: hit shapes, the per-tick history on the server, rewound ray, sphere-cast and overlap queries at a client's view tick, the
rewind limit and clamping, occlusion by the current static world, shooter and team filtering, validating client hit claims, and debugging.

Not in scope: moving physics bodies back in time (queries are pure functions over the history; nothing in the world moves), rewinding
non-hittable state (doors, platforms: tested at the current tick), client-side hit decisions without server validation, projectile flight
simulation (a projectile is a predicted spawn, spec 73 § 7; its impact uses these queries at its own tick).

## 2. Terms

| Term | Meaning |
|---|---|
| Hit shape | A sphere, capsule or box attached to an entity's root or to a skeleton joint, with a tag (`head`, `body`, `limb`). Used only for lag-compensated queries. |
| History | Server-side, per lag-compensated entity, the world-space pose of each hit shape and their combined bounding box, for each of the last N server ticks. |
| View tick | The fractional server tick a client was showing when it sampled a command (spec 74 § 8). |
| Rewind | Evaluating hit shapes at a past (fractional) tick from the history, by interpolating between the two recorded ticks around it. |
| Claim | A client's report "my shot hit target X at view tick V", which the server validates instead of tracing itself (§ 8). |

## 3. Components

```rust
/// Server and clients: this entity's hit shapes. Reflected, saved in prefabs.
#[derive(Component, Reflect, Default, Clone, Debug)]
#[reflect(Component, Default)]
pub struct HitShapes { pub shapes: Vec<HitShape>, pub pose_source: PoseSource }

#[derive(Reflect, Clone, Debug)]
pub struct HitShape {
    pub kind: HitShapeKind,          // Sphere { radius } | Capsule { radius, half_height } | Box { half_extents }
    pub attach: HitAttach,           // Root | Joint(Name)
    pub offset: Transform,           // relative to the attach point
    pub tag: Name,                   // "head", "body", ...; free-form, used by damage rules
}

#[derive(Reflect, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoseSource {
    /// Shapes follow the entity's transform only. Cheapest; joint-attached shapes use the bind pose.
    #[default]
    Root,
    /// Shapes follow sampled skeleton joints. The server must evaluate the entity's animation pose every tick.
    Joints,
}

/// Server: record history for this entity. Implied by `HitShapes` on a replicated entity unless `NoLagCompensation` is present.
#[derive(Component, Default)]
pub struct LagCompensated;

/// Opt-out: an entity with `HitShapes` that should not get history (a training dummy hit only at the present).
#[derive(Component, Reflect, Default)]
pub struct NoLagCompensation;
```

- With `PoseSource::Joints` the dedicated server runs animation pose evaluation (not skinning) for these entities each tick; the guide
  page names the cost and recommends `Root` with a few capsules for most games.
- A joint name that the skeleton lacks logs `net.hitshape_joint_missing` once per (prefab, name) and the shape falls back to the root.
- Validation on load: radius and half extents `> 0` and finite, at most 32 shapes per entity.

## 4. History

- At the end of each server fixed step (end of `FixedUpdate`, set `NetSet::LagCompRecord`, spec 70 § 8.1; there is no separate
  post-fixed schedule), the server records, for every `LagCompensated` entity, the world transform of each hit shape and the axis-aligned box enclosing all of them
  (inflated by the largest shape radius). Animation pose evaluation normally runs per frame; for `PoseSource::Joints` entities this set
  samples the entity's animation pose at the tick's time itself (pose evaluation only, through `regia_animation`'s sampling API), so the
  recorded joints match the tick, not the last rendered frame.
- The history is a ring of `ceil(max_rewind_ms / step_ms) + 2` ticks (default 250 ms at 60 Hz → 17 ticks). Entities that did not exist at a
  recorded tick have no entry for it.
- A teleport (spec 71 § 9) marks the entity's history entry for that tick as a cut: rewinding across the cut uses the nearest entry on the
  view tick's side, never an interpolation through the teleport.
- Despawned entities' history is kept until it ages out of the ring, so a shot at a target that died in the last 100 ms still resolves
  (the hit lands on a despawned entity: the query reports it with `alive: false`, and the game decides).

## 5. Queries

```rust
/// System parameter, server only. `Ray` is `regia_core::math::Ray` (origin + unit direction).
pub struct LagCompensation<'w> { /* ... */ }

impl LagCompensation<'_> {
    pub fn raycast(&self, shooter: NetPlayerId, ray: Ray, max_dist: f32, filter: &HitFilter) -> Option<LagHit>;
    pub fn sphere_cast(&self, shooter: NetPlayerId, ray: Ray, radius: f32, max_dist: f32, filter: &HitFilter) -> Option<LagHit>;
    pub fn overlap_sphere(&self, shooter: NetPlayerId, center: Vec3, radius: f32, filter: &HitFilter) -> Vec<LagHit>;
    /// Same queries at an explicit fractional tick (projectile impacts, replays of claims).
    pub fn at_tick(&self, tick: NetTickFrac) -> RewoundView<'_>;
    pub fn validate_claim(&self, shooter: NetPlayerId, claim: &HitClaim, muzzle: Vec3) -> ClaimVerdict;   // § 8
}

/// The same `raycast` / `sphere_cast` / `overlap_sphere` queries at a fixed tick, without a shooter: no shooter exclusion, no clamp
/// beyond the history range (a tick outside it returns `Err(LagCompError::TickOutOfHistory)` from each query).
pub struct RewoundView<'a> { /* ... */ }

pub enum ClaimVerdict { Valid(LagHit), Invalid(ClaimInvalid) }
pub enum ClaimInvalid { ViewTickOutOfRange, OriginTooFar, MissedShape, Occluded, TargetUnknown }

pub struct LagHit { pub entity: Entity, pub net_id: NetId, pub tag: Name, pub point: Vec3, pub normal: Vec3,
                    pub distance: f32, pub rewound_tick: NetTickFrac, pub alive: bool }

pub struct HitFilter { pub exclude: SmallVec<[Entity; 4]>, pub groups: Option<NetGroups>, pub predicate: Option<fn(&World, Entity) -> bool> }
```

Each query runs these steps:

1. **Rewind tick.** `t = NetViewTick(shooter)` (spec 74 § 8). Clamp to `[current_tick − max_rewind_ticks, current_tick]`, where
   `max_rewind_ticks = ceil(max_rewind_ms / step_ms)`; a clamp
   increments `rewind_clamped` for the shooter (a client reporting views older than the limit is either on a very bad link or cheating;
   the limit caps what it gains). Offline and on the listen-server host's own shots, `t = current_tick` (no rewind).
2. **Broad phase.** Every lag-compensated entity whose recorded boxes at the two ticks around `t` (union) intersect the query volume is a
   candidate. Entities in `filter.exclude`, the shooter's own controlled entities (always excluded), and entities failing `filter.groups`
   or `filter.predicate` are removed.
3. **Narrow phase.** Each candidate's shapes are posed at `t` by interpolating the two recorded transforms (position linear, rotation
   nlerp; across a cut, nearest). The query is tested against the posed shapes. The nearest hit wins; ties by `NetId`.
4. **Occlusion.** For rays and sphere casts, the current static and kinematic world (rapier query, `regia_physics`) is tested up to the
   hit distance, excluding the colliders of every lag-compensated entity and of the shooter's own entities (their present capsules are
   not where they were at `t`); if something solid is nearer, the query returns `None` (or, for `overlap_sphere`, drops hits whose line from the center is
   blocked). Dynamic entities that are not lag-compensated are tested at the current tick, as they are in the world now.
5. The result names the tag of the shape hit, so damage rules (headshots) live in game code.

- Queries are read-only: no component is written, no entity moves. They may run from any server system in `FixedUpdate`, in parallel.
- Querying on a client returns `Err(LagCompError::NotServer)`; client-side tracing for effects uses ordinary physics queries against the
  shown proxies (spec 74 § 9).

## 6. Fairness limits

- `max_rewind_ms` (default 250) bounds how far back a shot can be judged. Players on worse links than that are judged partly at the
  present (their shots get harder), which keeps "shot behind cover" situations short for the victim. The guide page explains the trade.
- The rewind uses the client's reported view tick only within the clamp; the server never trusts a view tick newer than its own tick.

## 7. Projectiles

- A projectile spawned by the server at the tick a command fired it (spec 73 § 7) advances on the server clock. Its impact test at tick T uses
  `at_tick(T − shooter_view_lag)`, where `shooter_view_lag = fire_tick − NetViewTick(shooter at fire time)` is stored on the projectile
  when it is spawned, during its first `projectile_rewind_ticks` ticks of flight (default 6), then the present: fast projectiles
  fired at a target the shooter saw are judged against what the shooter saw for the first part of the flight only. Games that want no
  rewind for projectiles set the value to 0.

## 8. Hit claims

For very high fire rates or shotgun patterns, sending every pellet's ray for the server to trace is expensive. A game may let the client
trace locally and send a **claim** (a to-server net message, spec 70) per hit:

```rust
#[derive(NetMessage)] // ToServer, Unreliable, rate-limited
pub struct HitClaim { pub target: Entity, pub tag: Name, pub ray: Ray, pub view_tick: NetTickFrac, pub weapon_seq: u32 }
```

- The server validates with `LagCompensation::validate_claim(shooter, &claim, muzzle)` (`muzzle` = the shooter's server-side muzzle
  point, computed by the game; the tolerances come from `config/net.ron`): the claim's view tick must be within the clamp
  of § 5 step 1 and within 2 ticks of the shooter's reported `NetViewTick`; the ray origin must be within `claim_origin_tolerance_m` (default
  0.5 m) of `muzzle`; the ray must hit the claimed target's claimed shape, inflated by
  `claim_tolerance_m` (default 0.1 m), at the rewound tick; and no static geometry may occlude it.
- Validation returns `Valid(hit)` or `Invalid(reason)`; the reasons' codes (for counters and the agent surface) are
  `view_tick_out_of_range`, `origin_too_far`, `missed_shape`, `occluded`, `target_unknown`. The game applies damage for `Valid` only. Invalid claims are counted per shooter; the counter is exposed for
  anti-cheat logic (no automatic kick; that is a game decision).

## 9. Files and formats

- `HitShapes` is a reflected component in prefabs: additive; after the v0.5.0 freeze a shape change needs a component version and a migrator.
- `config/net.ron` section `lag_compensation`:

```ron
lag_compensation: (
    max_rewind_ms: 250.0,
    projectile_rewind_ticks: 6,
    claim_tolerance_m: 0.1,
    claim_origin_tolerance_m: 0.5,
    max_shapes_per_entity: 32,
),
```

## 10. Determinism

- History recording and every query are pure functions of the server world at fixed steps and of the view ticks received. The same input
  recording and packet log give the same hits. Interpolation of shape poses uses the same `f32` operation order on every platform.
- Queries never depend on the order of entities in an archetype: candidates are sorted by `NetId` before the narrow phase.

## 11. Debugging

- Gizmo group `net_lagcomp` (server and listen-server host): for the last 16 queries, the rewound shapes at the rewind tick (green), the
  shapes at the current tick (grey), the ray and the hit point.
- Dev builds: the server can send the rewound shapes of a query to the shooting client (`lag_compensation.debug_echo: true`), and the client
  draws them over the proxy it showed, so a developer sees directly whether the shown and the judged pose agree.

## 12. Agent protocol surface

Guide page: `book/agents/net-lag-compensation.md` (topic `net-lag-compensation`; add its `book/COVERAGE.md` rows): why rewind exists, hit shapes and pose sources, the query
steps, the rewind limit trade-off, claims and their validation reasons, debug drawing, and every error code.

| Method | Mode | Params | Returns |
|---|---|---|---|
| `lagcomp.stats` | read (server) | — | `{ entities, ticks_kept, queries_per_s, mean_rewind_ms, clamped_per_s, claims_valid, claims_invalid_by_reason }` |
| `lagcomp.history` | read (server) | `{ entity }` | per recorded tick, each shape's tag and world pose |
| `lagcomp.raycast` | read (server, dev builds) | `{ peer, origin, direction, max_dist, tick? }` | `{ hit: LagHit \| null, rewound_tick }` |
| `lagcomp.last_queries` | read (server) | `{ limit? }` | the last queries with shooter, tick, result |

Errors (agent errors use `F0002` with `data.reason` set to the lowercase code, as neighboring built modules do; Decision (ADR-1585)): `net_not_server`, `peer_unknown`, `entity_not_lag_compensated`, `tick_out_of_history`, `entity_not_found`.

## 13. Tests and acceptance

Loopback hub, fixed steps, latency in ticks.

1. **The point of it.** A target strafes at 6 m/s; a client at 6-tick latency (and its adaptive interpolation delay) fires exactly at the
   target's shown center. With lag compensation the server reports a hit on `body` for 100 of 100 shots; with rewind disabled (test switch),
   the same shots miss at least 90 times.
2. **Fractional tick.** A shape posed at tick 10.5 sits exactly halfway between its tick-10 and tick-11 poses.
3. **Clamp.** A view tick older than the limit is clamped and counted; a view tick newer than the server tick is clamped to the present.
4. **Occlusion.** A wall built at the current tick between shooter and the rewound target blocks the hit.
5. **Shooter excluded.** A ray starting inside the shooter's own capsule hits the target behind, never the shooter.
6. **Teleport cut.** Rewinding across a teleport uses the nearest side's pose, never a point between the two locations.
7. **Dead target.** A target despawned 3 ticks ago is hit at its rewound pose with `alive: false`.
8. **Joints.** With `PoseSource::Joints`, a head shape follows an animated head joint; a missing joint falls back to the root and logs once.
9. **Claims.** One test per validation reason, plus a valid claim; claim counters per shooter.
10. **Projectile.** A fast projectile fired at a strafing target hits under `projectile_rewind_ticks: 6` and misses under 0 (documenting the
    option).
11. **Read-only.** After 1,000 queries, every component of every lag-compensated entity is byte-identical to before.
12. **Agent.** Every method round-trips; each error code is produced by a test.

## 14. Platform notes and performance budget

- The history lives on servers; dedicated servers are Linux and Windows (spec 76). An Android listen server records history for up to
  8 clients with `PoseSource::Root` (joint posing on Android servers is allowed but not budgeted).
- Budgets on the Desktop reference class (`06-testing-and-ci.md` § 3). Recording: 64 lag-compensated entities with 8 shapes each under 0.1 ms per tick (span `net_lagcomp_record`), excluding pose evaluation.
- Queries: 200 rewound raycasts per tick with 64 entities under 0.3 ms (span `net_lagcomp_query`); the broad phase keeps narrow tests to the
  few entities near each ray.
- Memory: `ticks_kept × entities × (shapes × 40 bytes + 24)`: 17 × 64 × 344 bytes ≈ 375 KiB at the defaults above.

## 15. Out of scope

Decision (ADR-1585): the `lag_compensation` section extends `config/net.ron` v1 (unfrozen until v0.5.0, serde defaults); `HitShapes` is an
additive reflected component, so no migrator is needed before the freeze.


Rewinding physics or doors; rewinding for melee arcs beyond `overlap_sphere` (games compose it); server-side "favor the victim" modes
other than the rewind limit; client hit markers before server confirmation (a game may show them predictively with `first_time()`, spec 73).

## 16. Provenance

- Set spec ids compared in review (ADR-1585): `runtime-networking-movement-prediction`, `runtime-networking-overview` (timing context only; no lag-compensation text exists in either, so the design is from the public sources below). Set spec ids used for timing context: `runtime-networking-movement-prediction`, `runtime-networking-overview` (behavior only, ADR-1526).
- Public sources: Y. Bernier, "Latency Compensating Methods in Client/Server In-game Protocol Design and Optimization", GDC 2001 (keeping
  a history of player positions and testing commands against the positions the client saw; a maximum rewind); Valve Developer Community
  wiki, the public lag compensation article; T. Ford, "Overwatch Gameplay Architecture and Netcode", GDC 2017 (rewinding hit volumes to
  the shooter's view, limits on rewind for players with high latency).
- Regia design choices: queries as pure functions with no world mutation, teleport cuts, despawned-target hits with `alive: false`, the
  claim validation reasons and tolerances, projectile rewind for the first ticks of flight, the defaults.

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
