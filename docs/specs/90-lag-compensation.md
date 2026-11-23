# 90 — Lag compensation: build record

Status: proposed (ADR-1752). Plan item D4 (`master-plan.md` § 12, "lag-compensation history for hit checks"), rung v0.5.0
(`versions.md`), `22-tier2.md` T2.3 ("lag compensation helper (server rewinds hitboxes)").

**Design source.** The reviewed design is spec 75 (`docs/specs/75-lag-compensation.md` on `integ/specs`, ADR-1599, review
ADR-1585). This file does not restate it: it records what the `lane/sprint-n2-lag-compensation` build implements, the
Rust API as built, where the build differs from spec 75 and why, and the acceptance status. Read spec 75 first. The file
number 90 keeps this record apart from spec 75 (which lands with `integ/specs`); when both are on `main`, this file's
§§ 3-5 fold into spec 75 as its "built" notes and this file is removed.

## 1. Purpose

A client shows remote players about 100 ms in the past (spec 74) and its shot reaches the server about 50 ms later. The
server keeps a short history of every hittable entity's hit shapes per server tick and tests shots against the shapes as
the shooter saw them, at the view tick the shooter reported (spec 74 § 8, `NetViewTick`).

## 2. Crate and files

| Item | Where |
|---|---|
| Components, history, queries, claims, agent data | `crates/regia_net/src/lagcomp.rs` (`regia_net::lagcomp`) |
| Shape geometry (ray, sphere-cast, overlap against sphere, capsule, oriented box) | `crates/regia_net/src/lagcomp/geom.rs` |
| Debug echo (`LagCompEcho`, `LagCompEchoLog`, `LagCompEchoPlugin`) | `crates/regia_net/src/lagcomp/echo.rs` |
| Physics occlusion, animator joints, gizmo group `net_lagcomp` | `crates/regia/src/lagcomp_bridge.rs` (`regia::lagcomp_bridge`, feature `net`) |
| Settings section `lag_compensation` | `crates/regia_net/src/settings.rs` (`LagCompSettings`) |
| Plugin | `regia_net::lagcomp::LagCompPlugin` (records in `NetSet::LagCompRecord`) |
| Agent methods | `crates/regia_agent/src/net_lagcomp.rs` |
| Guide page | `book/agents/net-lag-compensation.md` |
| Tests | `crates/regia_net/tests/lag_compensation.rs`, `crates/regia/tests/lagcomp_bridge.rs`, unit tests in `lagcomp.rs` and `geom.rs` |
| Bench | `crates/regia_net/examples/net_lagcomp_bench.rs` |

No new dependency: `regia_net` does not gain a `regia_physics` or `regia_animation` edge (§ 4 (b), (c)). The bridge sits
in the `regia` facade, which already depends on physics, animation and the renderer, next to `audio_occlusion`.

## 3. API as built

```rust
// Components (reflected; HitShapes saves in prefabs).
pub struct HitShapes { pub shapes: Vec<HitShape>, pub pose_source: PoseSource }
pub struct HitShape { pub kind: HitShapeKind, pub attach: HitAttach, pub offset: Transform, pub tag: Name }
pub enum HitShapeKind { Sphere { radius: f32 }, Capsule { radius: f32, half_height: f32 }, Box { half_extents: Vec3 } }
pub enum HitAttach { Root, Joint(Name) }
pub enum PoseSource { Root, Joints }
pub struct LagCompensated;      // marker, inserted by the recorder on replicated entities with HitShapes
pub struct NoLagCompensation;   // opt-out
pub struct LagCompProjectile { pub fire_tick: NetTick, pub shooter_view_lag: f64 }

// Resources.
pub struct LagCompHistory { .. }                 // the ring (server)
pub struct LagCompOcclusion(pub Option<OcclusionFn>);       // hook: static-world ray test
pub struct LagCompJointSource(pub Option<JointPoseFn>);     // hook: joint world poses
pub struct LagCompEchoLog { .. }                            // client: the newest echoes
pub type OcclusionFn = fn(&World, &Ray, f32 /*max_dist*/, &[Entity] /*ignore*/) -> Option<f32>;
pub type JointPoseFn = fn(&World, Entity /*root*/, &str /*joint*/) -> Option<Affine3A>;

// Queries: a read-only view over the server world.
impl<'w> LagCompensation<'w> {
    pub fn new(world: &'w World) -> Result<Self, LagCompError>;
    pub fn raycast(&self, shooter: NetPlayerId, ray: Ray, max_dist: f32, filter: &HitFilter) -> Result<Option<LagHit>, LagCompError>;
    pub fn sphere_cast(&self, shooter: NetPlayerId, ray: Ray, radius: f32, max_dist: f32, filter: &HitFilter) -> Result<Option<LagHit>, LagCompError>;
    pub fn overlap_sphere(&self, shooter: NetPlayerId, center: Vec3, radius: f32, filter: &HitFilter) -> Result<Vec<LagHit>, LagCompError>;
    pub fn at_tick(&self, tick: NetTickFrac) -> Result<RewoundView<'_, 'w>, LagCompError>;
    pub fn validate_claim(&self, shooter: NetPlayerId, claim: &HitClaim, muzzle: Vec3) -> Result<ClaimVerdict, LagCompError>;
    pub fn rewind_tick(&self, shooter: NetPlayerId) -> Result<NetTickFrac, LagCompError>;
    pub fn projectile_view_lag(&self, shooter: NetPlayerId) -> Result<f64, LagCompError>;  // now - rewind_tick, >= 0
    pub fn projectile_tick(&self, projectile: &LagCompProjectile) -> NetTickFrac;
    pub fn rewound_shapes(&self, entity: Entity, tick: NetTickFrac) -> Result<RewoundShapes, LagCompQueryError>;  // ADR-2551
}
pub enum LagCompError { NotServer, NotInstalled, PeerUnknown, TickOutOfHistory }  // codes: net_not_server, lagcomp_not_installed, peer_unknown, tick_out_of_history
pub enum LagCompQueryError { Query(LagCompError), NotLagCompensated }               // agent data calls: + entity_not_lag_compensated
pub struct LagCompAnimSource(pub Option<AnimCaptureFn>);    // hook: per-tick animation state (exact-tick joints, ADR-2551)
pub struct HitClaim { pub target: Entity, pub tag: String, pub origin: Vec3, pub direction: Vec3, pub view_tick: f64, pub weapon_seq: u32 }
pub struct LagCompEcho { pub server_tick: u32, pub rewound_tick: f64, pub origin: Vec3, pub direction: Vec3, pub shapes: Vec<EchoShape> }
pub struct LagCompEchoPlugin;                               // on every process

// regia::lagcomp_bridge (feature `net`).
pub struct LagCompBridgePlugin { pub occlusion_layers: Option<u32>, pub exact_tick_joints: bool }  // § 4 (h)
pub struct LagCompOcclusionLayers(pub Option<u32>);
pub struct NetLagCompGizmos;                                // group `net_lagcomp`
pub fn physics_occlusion(world: &World, ray: &Ray, max_dist: f32, ignore: &[Entity]) -> Option<f32>;
pub fn animator_joint(world: &World, root: Entity, joint: &str) -> Option<Affine3A>;
pub fn animator_state(world: &World, root: Entity, seconds: f32, previous: Option<&Arc<dyn RecordedAnim>>)
    -> Result<Arc<dyn RecordedAnim>, &'static str>;          // the exact-tick capture hook, ADR-2551
```

`LagHit`, `HitFilter`, `ClaimVerdict`, `ClaimInvalid` and `RewoundView` have the spec 75 § 5 shapes (the filter's
`exclude` is a `Vec<Entity>`: `regia_net` has no `smallvec` edge).

### 3.1 Settings (`config/net.ron`, `lag_compensation` section)

```ron
lag_compensation: (
    enabled: true,                // runtime switch: false judges every shot at the present tick
    max_rewind_ms: 250.0,
    projectile_rewind_ticks: 6,
    claim_tolerance_m: 0.1,
    claim_origin_tolerance_m: 0.5,
    claim_view_tick_slack: 2.0,   // ticks between a claim's view tick and the shooter's NetViewTick
    max_shapes_per_entity: 32,
    debug_echo: false,            // dev builds: send a shooter the rewound shapes of its hits
),
```

Milliseconds become ticks at the app's `FixedTime::hz` (60 when absent).

`format_version` stays 1: `config/net.ron` is unfrozen until v0.5.0 and every field has a serde default, so an older file
loads unchanged (no migrator, no `upgrade-project` step; `docs/formats/net-ron.md` gains the section). At the v0.5.0 freeze
the file is registered in `check-formats` with this section in it. `HitShapes` is an additive reflected component (no
component version before the freeze).

## 4. Differences from spec 75 (ADR-1752)

- (a) **Queries return `Result`.** Spec 75 § 5 lists `Option<LagHit>` returns and says client calls return
  `Err(LagCompError::NotServer)`; the build returns `Result<Option<LagHit>, LagCompError>` so both hold.
- (b) **Occlusion is a hook.** `regia_net` has no physics edge. `LagCompOcclusion` holds a function; the query passes the
  entities to ignore (every lag-compensated entity and the shooter's, sorted). `regia::lagcomp_bridge::physics_occlusion`
  is the engine's function (non-sensor colliders in the present `PhysicsWorld`, skipping those entities and their
  children, with an optional layer mask). With no hook installed nothing occludes and `lagcomp.stats` reports
  `occlusion: false`.
- (c) **Joint poses.** `PoseSource::Joints` reads each joint from `LagCompJointSource`, and when there is no hook or it
  returns `None`, from the root's scene hierarchy: the descendant entity whose scene `Name` equals the joint name, its
  `GlobalTransform` taken relative to the root's `GlobalTransform` and composed with the root's current transform (so a
  root that moved in this fixed step is not a frame late). `regia::lagcomp_bridge::animator_joint` is the engine's hook:
  the joint from the last evaluated `Animator` pose on the root or its nearest rigged descendant, with no named joint
  entities needed. Spec 75 § 4 samples at the tick's time; the build uses the last evaluation, because a pose graph holds
  playback state that a second evaluation per tick would advance (§ 4 (h)). A missing joint logs
  `net.hitshape_joint_missing` once per joint name and the shape follows the root.
- (d) **`LagCompensation` is built from `&World`**, not a system parameter: `fn system(world: &World)` systems run in
  parallel with other readers, the agent uses the same type, and the occlusion hook needs the world.
- (e) **`HitClaim` fields** are wire-codec types (`tag: String`, `origin` + `direction` for the ray, `view_tick: f64`);
  `HitClaim::ray()` gives the `Ray`. It is a to-server, unreliable net message with a rate limit of 240 per 60 ticks (claims
  are per hit: 8 pellets at 10 shots a second is 80 a second). Claims are rejected until the server holds a
  `HitClaimPolicy` (ADR-1892): `Validate(muzzle_fn)` judges them before delivery, `HandlerValidates` leaves it to the handler.
- (f) **Despawned targets** keep their history under their `NetId` until it ages out; `LagHit::alive` is `false` when the
  entity no longer exists or lost `HitShapes`. `NetGroups` are recorded per tick, so a dead target still passes a team
  filter; `filter.predicate` is not called for an entity that no longer exists.
- (g) **Teleport cuts.** `net_teleport` marks the entity's next recorded tick as a cut (`LagCompHistory::mark_cut`); the
  rewind across a cut takes the nearest recorded tick.
- (h) **Built behind a switch (ADR-2551):** with `LagCompBridgePlugin { exact_tick_joints: true }` the history keeps
  each `PoseSource::Joints` rig's animation state per tick (`LagCompAnimSource`, `RecordedAnim`) and a rewind
  evaluates the bone-attached shapes at the exact tick through `regia_animation::pose_eval` (side-effect-free, equal
  bit for bit to the live animator's pose for the same inputs). Off (the default), the joint is as old as the last
  evaluation, as before.
- (i) **Shooter's own entities.** Excluded: the shooter's `Player` entity (`NetPlayer`), every entity whose resolved owner is
  the shooter's peer, and `filter.exclude`. The occlusion ignore list also holds every entity with `NetOwner(shooter's
  peer)`, with hit shapes or without. Offline, and on the listen host's own shots, the rewind tick is the present.
- (j) **Recorded boxes.** Spec 75 § 4 inflates each box by the largest shape radius. The build records the exact box and
  adds a margin only where it matters: past a 45° turn between two ticks a blended pose can have corners outside both
  boxes, so the entry stores the largest bounding radius and the broad phase grows by it when blending into that tick.
  A constant inflation would send many more rays to the narrow phase for no gain on ordinary ticks.
- (k) **`debug_echo` needs `LagCompEchoPlugin`** on every process: registering the message changes the protocol hash, so a
  game opts in. "Dev builds" is `cfg!(debug_assertions)`. Each echo carries the query's ray and the shapes hit, at most 8
  a tick, on the unreliable channel. A client reads them through `lagcomp.echoes` and the gizmo group.
- (l) **Gizmo group.** Grey shapes are the newest recorded tick; the query's own tick has passed by the time it is drawn.
- (m) **`RewoundView` range check (ADR-5650).** Spec 75 § 5 has each `RewoundView` query return
  `TickOutOfHistory`; the build returns it once, from `LagCompensation::at_tick`, so a view that exists is always in range.
- (n) **Shape validation is at record time (ADR-5650).** Spec 75 § 3 says "validation on load". The recorder skips a
  shape with a non-positive or non-finite size and every shape past `max_shapes_per_entity`, and warns
  `net.hitshape_invalid` once per entity; a prefab with such shapes still loads (the editor can fix it).
- (o) **`HitShapes` is registered on every process (ADR-5650).** `NetPlugin` registers `HitShapes` and
  `NoLagCompensation` for reflection (client, server, listen host) and the editor's level types list them, so levels
  and prefabs keep their hit shapes everywhere; `LagCompPlugin` still adds the recorder on servers only.

## 5. Determinism

Recording and every query are pure functions of the server world at fixed steps and of the received view ticks.
History entries are kept sorted by `NetId`; candidates are tested in `NetId` order; the nearest hit wins, ties by `NetId`.
Interpolation is `lerp` on positions and `nlerp` (sign-corrected) on rotations in `f32`, with the blend factor computed
once in `f64` and narrowed. Counters are atomics; their totals do not depend on query order. `lagcomp.last_queries` is a
diagnostic ring whose order follows system execution order.

## 6. Agent protocol

As spec 75 § 12: `lagcomp.stats`, `lagcomp.history`, `lagcomp.raycast`, `lagcomp.last_queries`, plus `lagcomp.echoes` on
clients and `lagcomp.rewound_shapes { entity, tick }` on servers (ADR-2551: one entity's shapes posed at a tick as a query
tests them, with the recorded-animation counters). `lagcomp.raycast` and `lagcomp.rewound_shapes` are development-build
reads. Errors are `F0002` with `data.reason`: `net_not_running`, `net_not_server`, `net_not_client`, `peer_unknown`,
`entity_not_lag_compensated`, `tick_out_of_history`, `entity_not_found`, `lagcomp_not_installed`, and `lagcomp_dev_only`
(`lagcomp.raycast` or `lagcomp.rewound_shapes` in a shipping build). Guide page
`book/agents/net-lag-compensation.md`. The gizmo group is addressed by `debug.set_gizmos` as `net_lagcomp`.

## 7. Tests and acceptance

Spec 75 § 13 items, as built (`crates/regia_net/tests/lag_compensation.rs` unless noted):

| # | Test | Status |
|---|---|---|
| 1 | strafing target, 6-tick latency, shots at the shown center: 100/100 hit with rewind, >= 90 miss with `enabled: false` | built |
| 2 | fractional tick pose is exactly halfway | built |
| 3 | clamp of old and future view ticks, counted | built |
| 4 | occlusion by a hook wall | built |
| 5 | shooter excluded | built |
| 6 | teleport cut | built |
| 7 | despawned target, `alive: false` | built |
| 8 | joints follow a named child; missing joint falls back to the root | built (scene-hierarchy source) |
| 9 | each claim reason plus a valid claim, per-shooter counters | built |
| 10 | projectile rewind 6 vs 0 ticks | built |
| 11 | read-only after 1,000 queries | built |
| 12 | agent round-trip and every error code | built (`regia_agent` unit tests `lagcomp_methods_round_trip_and_every_reason`, `lagcomp_echoes_on_a_client`) |
| - | ties go to the lower `NetId` whatever the spawn order | built (`ties_go_to_the_lower_net_id`) |
| - | shapes past `max_shapes_per_entity` are not recorded and cannot be hit | built (`shapes_past_max_shapes_per_entity_are_not_recorded`) |
| - | clients refused, `NoLagCompensation`, invalid shapes skipped | built (`client_opt_out_and_invalid_shapes`) |
| - | the plugin records each fixed step, ring of 17 ticks at 60 Hz | built (`plugin_records_each_fixed_step`) |
| - | a `HitClaim` sent over the loopback hub resolves its entity and validates | built (`claims_travel_and_validate_on_the_server`) |
| - | ray, sphere-cast and overlap geometry (caps, rotated shapes, rounded box corners) | built (`lagcomp::geom` unit tests) |
| - | a teleport on the unrecorded present tick is a cut | built (`teleport_on_the_present_tick_is_a_cut`) |
| - | a claim with a zero or non-finite direction misses, even from inside the shape | built (`claims_without_a_direction_miss`) |
| - | a 90° turn between ticks keeps its corners in the broad phase | built (`a_fast_turn_keeps_its_corners_in_the_broad_phase`) |
| - | a dead target passes group filters; a predicate never sees a dead entity | built (`a_dead_target_passes_group_and_predicate_filters`) |
| - | a shape on a child follows a parent moved in the same fixed step | built (`a_child_follows_its_moving_parent_in_the_same_step`) |
| - | everything the shooter owns is on the occlusion ignore list, sorted | built (`the_occlusion_hook_ignores_the_shooters_own_entities`, `the_shooters_own_collider_does_not_occlude_it`) |
| - | the debug echo reaches the shooter, not for misses or the listen host, bounded log | built (`the_debug_echo_reaches_the_shooter`, `echoes_are_bounded_and_skip_the_host`) |
| 4 | a present physics wall occludes; sensors and targets' own colliders do not; layer mask | built (`crates/regia/tests/lagcomp_bridge.rs`) |
| 8 | joint shapes follow an animator's pose, on the root or a child rig; missing joint falls back | built (`crates/regia/tests/lagcomp_bridge.rs`) |
| - | the gizmo group is named, off by default, and draws hits and misses | built (`the_gizmo_group_draws_what_a_query_judged`; no golden, line counts on a `GizmoBuffer`) |
| - | at most `max_shapes_per_entity` shapes are recorded and hittable (§ 3) | built (`shapes_past_the_per_entity_cap_are_not_recorded`, ADR-5650) |
| - | an entity has no history before the tick it spawned (§ 4) | built (`an_entity_has_no_history_before_it_spawned`, ADR-5650) |
| - | `HitShapes` saved in a level loads on client, dedicated and listen processes (§ 3, § 9) | built (`hit_shapes_save_in_a_level_and_load_on_every_process`, ADR-5650) |

## 8. Platforms

Pure Rust over `glam` and the ECS: Linux, Windows and Android aarch64 build the same code (no `cfg(target_os)`). History
lives on servers; dedicated servers are Linux and Windows; an Android listen server records for up to 8 clients with
`PoseSource::Root`. macOS builds as a testing target only.

## 9. Performance budget

Spec 75 § 14: record 64 entities x 8 shapes under 0.1 ms per tick (span `net_lagcomp_record`); 200 rewound raycasts with 64
entities under 0.3 ms (span `net_lagcomp_query`). `net_lagcomp_bench` measures both, on the CPU; there is no GPU work.

| 600 ticks, release, `net_lagcomp_bench` | Mean | Median | Worst | Budget |
|---|---|---|---|---|
| Record | 0.034 ms | 0.031 ms | 0.155 ms | 0.1 ms |
| 200 raycasts | 0.220 ms | 0.217 ms | 0.325 ms | 0.3 ms |

Measured on a shared build server at load 80 with the bench at `nice 19`, 2026-10-30. Means and medians are inside the
budget. Worst ticks are not, and this host cannot tell scheduling from cost: run it on an idle Desktop reference machine
before calling the budget met. The budget also leaves out the occlusion hook, which is the host's physics query.

## 10. Out of scope

As spec 75 § 15, plus the deferrals of § 4 (h).
