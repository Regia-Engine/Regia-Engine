# Net lag compensation (agents)

Server-side rewind of hit shapes for shots in `regia_net::lagcomp` (spec 75, build record spec 90, ADR-1752). Guide
topic `net-lag-compensation`.

## Why rewind exists

A client shows other players a short interpolation delay in the past (about 100 ms, [net
interpolation](net-interpolation.md)), and its shot reaches the server half a round trip later. If the server tested
the shot against where targets are *now*, a player who aimed exactly at what they saw would miss a moving target by
the target's speed times that whole delay (at 6 m/s and 150 ms, almost a metre). Lag compensation keeps a short
history of where every hittable entity's **hit shapes** were on each server tick, and tests the shot against the
shapes **as the shooter saw them**, at the view tick the client reported with its input (`NetViewTick`).

Nothing in the world moves back in time: queries read the history (and the live world for the present tick) and
write no component.

## Turning it on

- Add `LagCompPlugin` after `NetPlugin` on every process that may be a server (it does nothing on clients). Worlds
  without an `App` call `install_lag_compensation(world)` and run `lagcomp_record(world)` at the end of each fixed
  step.
- Give hittable entities `HitShapes`. Every replicated entity with `HitShapes` gets `LagCompensated` at its first
  recorded tick; `NoLagCompensation` opts out (a training dummy judged only at the present).
- The recorder runs in `NetSet::LagCompRecord` (end of `FixedUpdate`, after gameplay; span `net_lagcomp_record`) and
  keeps `ceil(max_rewind_ms / step_ms) + 2` ticks: 62 at 60 Hz with the default 1,000 ms (spec 120 § 9.6; 17 with
  the 250 ms default before v0.6).
- Switch rewind off at runtime with `lag_compensation.enabled: false` in `NetSettings` (every shot is then judged at
  the present; the history keeps recording, so switching back is immediate).
- A game built on the `regia` crate with its `net` feature adds `regia::lagcomp_bridge::LagCompBridgePlugin` after
  `LagCompPlugin`. It installs the physics occlusion and animator joint hooks below (a hook the host already set is
  kept) and the `net_lagcomp` gizmo group.
- The view tick comes from the shooter's input: `set_view_tick` checks it against the shooter's measured window
  (below) and stores the checked tick in `NetViewTick` when a command is consumed. With no view tick a shooter is
  judged at the present.

## Hit shapes and pose sources

`HitShapes { shapes, pose_source }`; each `HitShape` has a `kind` (`Sphere { radius }`, `Capsule { radius,
half_height }` along local Y, `Box { half_extents }`), an `attach` (`Root` or `Joint(name)`), an `offset` from the
attach point (translation and rotation; scale is ignored) and a `tag` (`head`, `body`, `limb`: free-form, returned
with every hit so damage rules live in game code). Sizes are in metres; entity scale does not scale them. An entity may sit under a moving parent: its pose is composed
from local transforms, so it is not a frame late. At most
`max_shapes_per_entity` (32) shapes are recorded; a non-positive or non-finite size is skipped. Both log
`net.hitshape_invalid` once per entity.

- `PoseSource::Root` (default, cheapest): every shape follows the entity transform; joint-attached shapes sit at their
  offset from the root. Recommended for most games: a few capsules.
- `PoseSource::Joints`: joint-attached shapes follow the joint. The pose comes from the `LagCompJointSource` hook (a
  function that returns the joint's world transform at the tick being recorded). `LagCompBridgePlugin` installs
  `animator_joint`, which reads the joint from the `Animator` on the root (or the nearest descendant with one) and
  needs no named joint entities. When there is no hook, or it returns `None`, the recorder looks in the scene
  hierarchy: the descendant entity whose `Name` equals the joint name, taken relative to the root and composed with
  the root's current transform. Either way the local pose is as fresh as the last animation evaluation (the root is
  current); a host that needs the exact tick samples its own pose in the hook. A joint that is not found logs
  `net.hitshape_joint_missing` once per joint name and the shape follows the root. Joint posing walks the skeleton or
  the hierarchy per shape per tick; it is not budgeted on Android listen servers.

### Exact-tick joints

`LagCompBridgePlugin { exact_tick_joints: true, .. }` (off by default, ADR-2551) installs `animator_state` as the
`LagCompAnimSource` hook. The history then keeps, for each `PoseSource::Joints` rig, the animation *state* of every
tick (clip clocks, state-machine states and transitions, layer weights, the parameters and the step's time; no pose),
and a rewound query that reaches the entity evaluates its bone-attached shapes at that exact server tick through
`regia_animation::pose_eval` (a side-effect-free evaluation that gives the pose the live animator produces for the
same state, parameters and step, bit for bit). Without it the joints are as old as the last animation frame.

- Recording evaluates nothing: the entity's box is its root position grown by the rig's reach (1.5 times the longest
  bind-pose chain, plus each shape's extent). A posed shape found outside that box is counted (`reach_exceeded` in
  `lagcomp.rewound_shapes`) and logged once as `net.lagcomp_reach_exceeded`; a rig with long translation tracks
  can need its own hook.
- Every query of one tick shares one evaluation per entity and record (span `net_lagcomp_pose_eval`).
- Memory: the state of the demo character (20 joints, eight shapes, a state machine, a blend space, a layer) stays
  under spec 75's `17 × (8 × 40 + 24)` bytes per entity (`LagCompHistory::entity_bytes`).
- A despawned target poses from its recorded state (the record holds the skeleton and the graph).
- What the state cannot hold falls back to the posed-shape path for that tick, counted by reason in `anim.fallbacks`:
  `no_rig`, `montage_active` (a montage blends after the graph), `anim_not_evaluated` (the first frame),
  `anim_inertialized` (an inertialized transition's offset is a whole pose), `anim_sync_groups`,
  `anim_shape_mismatch`. IK targets are not rewound (the pose before IK).
- The step's time is one fixed step from the animator's last evaluation, with its speed: exact when the server
  evaluates animation once per tick (a dedicated server's frame is its tick), within a frame otherwise.

## Queries

`LagCompensation::new(&world)` gives a read-only view (systems that take `&World` run in parallel with other
readers):

| Query | Returns |
|---|---|
| `raycast(shooter, ray, max_dist, &filter)` | nearest `LagHit` or `None` |
| `sphere_cast(shooter, ray, radius, max_dist, &filter)` | nearest hit of a swept sphere (occlusion tests the center line) |
| `overlap_sphere(shooter, center, radius, &filter)` | every entity touching the sphere, in `NetId` order |
| `at_tick(tick)` → `RewoundView` | the same three at an explicit tick, no shooter (projectiles, replays) |
| `validate_claim(shooter, &claim, muzzle)` | `Valid(hit)` or `Invalid(reason)` |
| `rewind_tick(shooter)`, `projectile_view_lag(shooter)`, `projectile_tick(&projectile)` | the ticks used |

Each query runs these steps:

1. **Rewind tick.** The shooter's `NetViewTick`, clamped to `[current − max_rewind_ticks, current]` (and to the oldest
   recorded tick). A clamp is counted for the shooter. Offline, on the listen host's own shots (`PeerId::SERVER`),
   with no view tick yet, or with `enabled: false`, the tick is the present.
2. **Broad phase.** Entities whose recorded boxes on the two ticks around the rewind tick touch the query.
   `filter.exclude`, the shooter's `Player` entity, every entity owned by the shooter's peer (resolved `NetOwner`),
   and entities failing `filter.groups` (at least one shared `NetGroups` name, as recorded at that tick, so a target
   that has since died still passes) or `filter.predicate` are removed. The predicate is not called for an entity
   that no longer exists.
3. **Narrow phase.** Shapes are posed at the fractional tick: position `lerp`, rotation `nlerp` on the short arc.
   Across a **teleport cut** (`net_teleport` on that tick), or when the shape list changed, the nearest recorded tick
   is used, never a point between. An entity present on only one of the two ticks is used only when that tick is
   the nearer one. The nearest hit wins; equal distances go to the lower `NetId`.
4. **Occlusion.** With a `LagCompOcclusion` hook installed, the static world is tested up to the hit distance; the
   hook is told to ignore every lag-compensated entity and everything the shooter owns, hit shapes or not (present
   colliders are not where they were, and a shot starts inside its shooter's body; the list is sorted). A nearer solid
   hit drops the result. `overlap_sphere` drops hits whose line from the center is blocked. Without a hook nothing
   occludes (`lagcomp.stats.occlusion` is `false`).
5. The hit names the shape's `tag`. `alive: false` means the entity was despawned (or lost `HitShapes`) since: a
   target that died in the last 100 ms still resolves, and the game decides.

Querying on a client returns `Err(LagCompError::NotServer)`; client-side effect traces use ordinary physics queries
against the shown proxies.

`LagCompBridgePlugin` installs `physics_occlusion`: the nearest non-sensor collider in the present `PhysicsWorld`,
skipping the listed entities and their children. Its `occlusion_layers` bitmask (`1 << layer_index`) limits what stops
a shot, so bullets can pass through glass:

```rust,wrap
# use regia::prelude::*;
# use regia_net::lagcomp::LagCompPlugin;
# use regia::lagcomp_bridge::LagCompBridgePlugin;
# let app: &mut App = todo!();
# const WORLD_LAYER: u32 = 0;
app.add_plugins(LagCompPlugin);
app.add_plugins(LagCompBridgePlugin {
    occlusion_layers: Some(1 << WORLD_LAYER),
    exact_tick_joints: false, // true: rewind joint rigs at the exact tick (ADR-2551)
});
```

A host with its own physics installs a function instead (`regia_net` itself has no physics dependency):

```rust,ignore
fn occlusion(world: &World, ray: &Ray, max_dist: f32, ignore: &[Entity]) -> Option<f32> {
    my_physics_raycast(world, ray, max_dist, |owner| ignore.binary_search(&owner).is_err())
}
world.resource_mut::<LagCompOcclusion>().0 = Some(occlusion);
```

## The rewind limit

Each shooter has its own window, worked out by the server from what it measured for that shooter (spec 120 § 9.6,
ADR-8315), every time one of its commands is consumed:

```text
expected age = RTT + input lead + interpolation delay + 1 tick
window       = min(expected age + allowance, max_rewind_ms)
allowance    = 50 ms + 2 × RTT variation
```

- **RTT** and **RTT variation**: the transport's smoothed round trip for the shooter's peer (the server endpoint's
  peer report, as in `net.status`), or what a host set with `lagcomp::set_shooter_rtt(world, peer, rtt_ms,
  jitter_ms)` when its transport publishes no report.
- **Input lead**: the averaged arrival margin of the shooter's commands (`net.input_buffer` `margin`), at most
  `prediction.margin_max_ticks`.
- **Interpolation delay**: the delay the command reported, but no more than the server's model of the shooter's
  interpolation clock allows: the target delay for the server's send rate and the measured variation
  (`delay_send_intervals` sends + 4 × variation, inside `min_delay_ms..max_delay_ms`), which falls no faster than a
  client's clock may lower its delay (`lower_rate`). The model starts at `max_delay_ms` when the shooter joins.

The carried view tick must lie in `[now − window, now]` and may not run backward by more than the allowance from the
shooter's previous view. A tick outside is clamped to the nearest allowed tick and counted for the shooter
(`lagcomp.stats` `windows`: `clamped_old`, `clamped_future`, `clamped_backward`); every rewound query and hit claim
then uses the clamped tick, so a claim at the unclamped tick is refused (`view_tick_out_of_range`). A shooter the
server has not measured gets `max_rewind_ms` as its window (`measured: false`); the future and backward checks still
apply. The listen host's own players and offline worlds are judged at the present as before.

`max_rewind_ms` (default **1,000**, at most 1,000; `lagcomp::MAX_REWIND_CEILING_MS`) is the project maximum on every
window. With per-shooter windows a short-link player cannot use it: a 5 ms player's window is about 150 ms, so a
claimed 250 ms-old view is posed about 150 ms back. It decides only how far the longest links reach: at the defaults a
shooter is judged exactly where it aimed up to a round trip of about 800 ms (the high-rtt profile's 450 and 650 ms
clients are). **The fairness tradeoff:** a larger maximum lets players on long links hit what they saw, and makes
their targets more often hit after they reached cover (up to the window late); a smaller one does the opposite, and
a shooter past it is judged partly at the present (its shots get harder). Lower it in `config/net.ron`
(`lag_compensation.max_rewind_ms`) for a game that favors targets; spec 120 owner question Q5 proposes 400 ms. The
history keeps `ceil(max_rewind_ms / step_ms) + 2` ticks. The server never trusts a view tick newer than its own tick.

Watch it with `lagcomp.stats` `windows` (per shooter: `measured`, `rtt_ms`, `jitter_ms`, `lead_ms`, `interp_ms`,
`interp_bound_ms`, `expected_age_ms`, `allowance_ms`, `window_ms`, `last_view_tick`, `last_server_tick`, `accepted`
and the three clamp counts), `clamped_per_s` and `mean_rewind_ms`. A shooter whose `clamped_old` climbs on a short
link reports views older than its link explains: a tampered client, or an interpolation delay forced past the
server's model (`net.set_interp_delay`).

## Projectiles

Spawn the projectile with `LagCompProjectile { fire_tick, shooter_view_lag: lc.projectile_view_lag(shooter)? }`.
Each tick, test its swept segment with `lc.at_tick(lc.projectile_tick(&projectile))?`: during its first
`projectile_rewind_ticks` (6) ticks of flight it is judged against what the shooter saw, then at the present. Set
`projectile_rewind_ticks: 0` for no projectile rewind.

## Hit claims

For high fire rates or shotgun patterns a game may trace on the client and send a `HitClaim { target, tag, origin,
direction, view_tick, weapon_seq }` per hit (a to-server, unreliable net message, 240 per 60 ticks; register it on
every process with `app.add_net_message::<HitClaim>()`; send it with `NetSender::send_to_server` targeted at the
shooter's own pawn, since to-server messages must target an entity the sender owns, and name the hit entity in
`target`). The server checks it with `validate_claim(shooter, &claim,
muzzle)`, where `muzzle` is the server's own muzzle point.

Claims are rejected until the server world holds a `HitClaimPolicy`, so a game that registers the type and nothing else
trusts no client. `HitClaimPolicy::Validate(muzzle_fn)` has the engine judge every claim before delivery: handlers of
`NetMessages<HitClaim>` see valid claims only. `HitClaimPolicy::HandlerValidates` delivers claims unchecked and the
handler must call `validate_claim` itself. Refused claims count as `rejected` in `net.messages`.

| Verdict | When |
|---|---|
| `view_tick_out_of_range` | the claim's view tick is outside the rewind window, newer than the server tick, or more than `claim_view_tick_slack` (2) ticks from the shooter's `NetViewTick` |
| `origin_too_far` | the ray starts more than `claim_origin_tolerance_m` (0.5 m) from `muzzle` |
| `target_unknown` | the target has no history at the claimed tick (or is the shooter's own) |
| `missed_shape` | the ray misses every shape with the claimed tag, grown by `claim_tolerance_m` (0.1 m), at the claimed tick |
| `occluded` | the occlusion hook finds static geometry before the shape |

Apply damage for `Valid` only. Verdicts are counted per shooter (`LagCompHistory::shooter_counters`, `lagcomp.stats`
`shooters`) for anti-cheat logic; nothing kicks automatically. With `enabled: false` claims are judged at the
present.

## Debugging

`lagcomp.last_queries` returns the last 16 queries with the shooter, the server and rewound ticks, the clamp flag,
the ray, the hit and the **rewound shapes of the entity hit**.

- **Gizmo group `net_lagcomp`** (off by default; `debug.set_gizmos`, needs `LagCompBridgePlugin`): for those queries
  the rewound shapes in green, the same entity on the newest recorded tick in grey, the ray and the hit point; a miss
  draws only its ray. On a client it draws the echoed shapes in magenta.
- **Debug echo.** With `lag_compensation.debug_echo: true`, in a dev build (`debug_assertions`), the server sends a
  shooter the rewound shapes of its hits, so a developer sees over the proxy they aimed at whether the shown and the
  judged pose agree. Add `LagCompEchoPlugin` on every process (the message feeds the protocol hash). A release build
  ignores the setting. At most 8 hits a tick are echoed; a client keeps the newest 16 (`lagcomp.echoes`).

## Settings

`config/net.ron` section `lag_compensation` (`format_version` 1, unfrozen until v0.5.0, every field has a default):
`enabled: true`, `max_rewind_ms: 1000.0` (250.0 before v0.6; at most 1,000), `projectile_rewind_ticks: 6`, `claim_tolerance_m: 0.1`,
`claim_origin_tolerance_m: 0.5`, `claim_view_tick_slack: 2.0`, `max_shapes_per_entity: 32`, `debug_echo: false`. The
step rate is the app's `FixedTime::hz` (60 when absent).

## Methods

| Method | Mode | Params | Returns |
|---|---|---|---|
| `lagcomp.stats` | read (server) | — | `{ entities, ticks_kept, range, max_rewind_ticks, queries_per_s, mean_rewind_ms, clamped_per_s, claims_valid, claims_invalid_by_reason, enabled, occlusion, joint_source, windows, shooters }` |
| `lagcomp.history` | read (server) | `{ entity }` | `{ entity, ticks: [{ tick, cut, shapes: [{ tag, kind, position, rotation, size }] }] }` |
| `lagcomp.raycast` | read (server, dev builds) | `{ peer, local?, origin, direction, max_dist?, tick? }` | `{ hit: LagHit \| null, rewound_tick }` |
| `lagcomp.last_queries` | read (server) | `{ limit? }` | `{ queries: [{ kind, shooter, server_tick, rewound_tick, clamped, origin, direction, radius, hit, shapes, verdict }] }` |
| `lagcomp.echoes` | read (client) | `{ limit? }` | `{ echoes: [{ server_tick, rewound_tick, origin, direction, shapes: [{ tag, kind, position, rotation, size }] }] }` |
| `lagcomp.rewound_shapes` | read (server, dev builds) | `{ entity, tick }` | `{ entity, net_id, tick, source, alive, shapes: [{ tag, kind, position, rotation, size }], anim: { entities, poses, failed, reach_exceeded, fallbacks, history_bytes } }` |

- `queries_per_s` and `clamped_per_s` cover the last second of recorded ticks; `mean_rewind_ms` covers every query.
  A high `clamped_per_s` means players report views older than the limit: bad links or a tampered client.
- `windows` lists each shooter's measured rewind window (spec 120 § 9.6, `lagcomp::ShooterWindow`) once one of its
  commands carried a view tick; `shooters` lists the per-shooter query and claim counters.
- `lagcomp.raycast` without `tick` behaves exactly like a game's `raycast` for that peer's player (and is counted
  and logged); with `tick` it runs at that tick without shooter exclusion.
- `lagcomp.history` keeps a despawned entity's ticks until they age out.
- `lagcomp.rewound_shapes` poses one entity's shapes at a fractional tick with the same bracket, blend and teleport
  rules a query uses. `source` is `anim_state` when they were evaluated from the recorded animation state (exact-tick
  joints) and `recorded` when they come from recorded shape poses; `anim` gives the history's counters (entities
  recorded as state on the newest tick, evaluations, failed evaluations, shapes outside their box, fallbacks by
  reason) and `history_bytes`, this entity's history in bytes. Example: `{"method": "lagcomp.rewound_shapes",
  "params": {"entity": 4294967310, "tick": 120.5}}`.

## Errors

Code `F0002` with `data.reason`:

| Reason | When |
|---|---|
| `net_not_running` | the app has no `regia_net` runtime |
| `net_not_server` | the process is a client |
| `net_not_client` | `lagcomp.echoes` on a server or offline process |
| `lagcomp_not_installed` | the server has no `LagCompHistory` (`LagCompPlugin` not added), or the client has no `LagCompEchoLog` (`LagCompEchoPlugin` not added) |
| `peer_unknown` | `lagcomp.raycast` names a peer that is not connected |
| `entity_not_found` | the entity does not exist and has no history |
| `entity_not_lag_compensated` | the entity exists but has no recorded history |
| `tick_out_of_history` | `lagcomp.raycast` or `lagcomp.rewound_shapes` with a `tick` outside the recorded range |
| `lagcomp_dev_only` | `lagcomp.raycast` or `lagcomp.rewound_shapes` in a shipping build |
