# Destruction level 2: chunks, bonds, collapse

`regia_physics::destruction` breaks objects along lines decided ahead of time. An object is a set
of pre-fractured chunks held together by bonds (ADR-1597). Hits load the bonds; a bond breaks when
its accumulated impulse reaches its threshold; chunks that lose every path to a chunk the world
holds up collapse. Detached chunks become pooled rapier debris bodies under hard caps. Runtime
fracture and carving (level 3) are not part of this.

| Method | Permission | What it does |
|---|---|---|
| `physics.destruction_define` | edit scene | Place destructibles: the wall-and-bridge sample, a box grid, or inline `.destructible.ron`. |
| `physics.destruction_damage` | play | Queue an explosion, projectile or impulse for the next fixed tick. |
| `physics.destruction_state` | read | Chunks, bonds, hashes, counters, the last tick, recent break events. |
| `physics.destruction_budget` | play | Read or change the caps; report the debris pool. |
| `physics.destruction_load_report` | read | Solve the structural load now: the bonds nearest their limit, what each support carries (ADR-2734). |

All of them need `DestructionPlugin` (with `PhysicsPlugin`). Without it they answer `F0003` (not
found) naming the plugin.

## Making a destructible

From the command line, one step:

```text
regia fracture props/wall.glb --out props/wall --chunks 32 --seed 7 --fx-tag concrete
regia fracture --box 1,0.5,0.25 --out props/slab --chunks 24
regia fracture props/wall.fracture.ron --out props/wall_heavy --density 2600 --bond-seconds 0.02
```

The input is a closed `.glb` (its first mesh, turned to Regia's -Z forward), `--box X,Y,Z` half
extents, or an existing `.fracture.ron` (cook only). It writes `<out>.fracture.ron` (chunk meshes and
joints) and `<out>.destructible.ron` (what the game loads), each through a temporary file and a
rename. Options: `--chunks`, `--seed`, `--strength-per-area` (N/m^2 of joint contact), `--density`,
`--bond-seconds`, `--support-below-y Y` or `--free`, `--interior-uv-scale`, `--fx-tag`, `--kit ID --kit-revision REV` (names
the chunks `frac_<kit>_<n>` and records the source), `--json` (a summary for tools).
Errors print the fracture or destruction error code.

From code:

1. Fracture a closed mesh with the fracture tool (`regia_asset_pipeline::fracture`, see
   [Pre-fractured destruction](fracture.md)): `fracture_mesh` gives a `.fracture.ron` with the chunk
   meshes and the joints between touching chunks.
2. Cook it into the runtime asset: `regia_asset_pipeline::destructible_cook::cook_destructible(&set,
   &CookSettings)`. Settings: `density` (kg/m^3, default 2400), `bond_seconds` (a joint's break force
   times this is the bond's break impulse, default 0.01 s), `support` (`BelowY(y)`: chunks whose
   lowest point is at or below `y`; the default is the set's lowest centimetre; `Explicit` indices;
   `None` for a free object), `material` and `meshes` (the `.fracture.ron` path for rendering).
3. Save `asset.to_ron()` as `name.destructible.ron` (format: `docs/formats/destructible.md`).

Per chunk the asset can also carry, for content made outside the fracture tool: a stable `name`, the
kit `parent` part, several convex `hulls` (one compound collider), explicit principal `inertia`,
render `lods`, an `fx_tag` override (a glass pane in a concrete wall), `small` (no debris body, only
the effect hook), `lifetime_secs`, and `edge_heat` for hot cut edges. Per set: the `source` kit and
revision, `fracture_seed`, `pattern_id`, `replaces_stage` (a damage stage the set stands in for),
and `outer_material` / `inner_material`.

Walls, floors, decks and piles of boxes need no fracture: `DestructibleAsset::box_grid(&BoxGrid {
counts, cell, density, strength_per_area, support, material, .. })` builds a grid of box chunks with
bonds between face neighbours (`GridSupport::Bottom`, `Ends`, `None` or `Explicit`).

In the game:

```rust,wrap
# use regia::prelude::*;
# use std::sync::Arc;
# use regia_physics::{PhysicsPlugin, PhysicsWorld};
# use regia_physics::destruction::{
#     Damage, DamageKind, DestructibleAsset, DestructiblePose, Destruction, DestructionPlugin,
# };
# let app: &mut App = todo!();
# let destruction: &mut Destruction = todo!();
# let physics: &mut PhysicsWorld = todo!();
# let asset: DestructibleAsset = todo!();
# let seed: u64 = 7;
app.add_plugins((PhysicsPlugin, DestructionPlugin::default()));
// Later, with &mut Destruction and &mut PhysicsWorld:
let id = destruction.define(physics, Arc::new(asset), DestructiblePose::at([0.0, 0.0, -5.0]), seed)?;
destruction.world.damage(Damage {
    target: None,
    kind: DamageKind::Explosion { center: [0.0, 1.0, -4.6], radius: 1.2, impulse: 1500.0 },
})?;
```

## How hits break things

- **Explosion** `{ center, radius, impulse }`: every attached chunk whose centre is within `radius`
  gets `impulse * (1 - distance / radius)`, pushed away from the centre.
- **Projectile** `{ origin, direction, impulse, radius = 0, max_distance = 100 }`: the ray hits the
  nearest attached chunk's bounds; chunks within `radius` of the hit point get the falloff impulse
  along the ray (the hit chunk at least half). A shot that hits nothing counts as a miss.
- **Impulse** `{ id, chunk, vector }`: one chunk gets the impulse (a vehicle, a melee hit, a
  contact force the game measured: `DestructionPhysics::chunk_of_collider` maps a raycast or contact
  collider back to `(id, chunk)`).

Each impulse is scaled by the material's `damage_scale`; one below `min_impulse` does nothing. A
chunk's impulse is shared equally by its intact bonds, so a chunk with fewer bonds left breaks
loose sooner. Bonds keep the damage (`decay_per_second` heals it). At the end of the tick, bonds at
or over `break_impulse` break (at most `max_breaks_per_tick`, in instance then bond order; the rest
break on later ticks), and then the support graph runs: attached chunks with no path of intact bonds
to a `support` chunk detach (`collapse`); a chunk knocked loose by this tick's hit detaches with
cause `damage`. An object without support chunks keeps its largest piece. A detached chunk's
remaining bonds break with it, so a falling section comes apart into its chunks.

## Structural load (ADR-2734)

An asset with `load` settings (`.destructible.ron` v2) carries its own weight. Each chunk's weight
(`mass * gravity`) flows through the intact bonds to the support chunks, the way current flows
through a resistive network to ground: each bond conducts in proportion to its area, the flows
conserve weight exactly, a column's bond carries the weight above it and an arch splits its load
between its feet. The flows are solved with `iterations` conjugate-gradient steps (default 64; a
chain of `n` loose chunks is exact after `n`), in `f64` with fixed summation order, only on a tick
where the load-carrying bonds changed (or after `DestructionWorld::resolve_load`).

A bond whose load is over its `max_load` (or `load.max_load_per_area * area`) gains
`(load - max_load) * failure_rate * dt` newton-seconds of damage each tick and breaks at its
`break_impulse`, exactly like damage from hits, so the two add up. Breaking one bond moves its load
to the others: knock a leg out from under a table and the remaining leg's joint fails a few ticks
later, then the top collapses. A bond that reaches its threshold is decided at once and stops
carrying load; the per-tick cap (`max_breaks_per_tick`) only delays when its event goes out. Decided
bonds leave oldest first and the support graph runs when the last bond decided on one tick has
broken, so a capped cascade breaks the same bonds, and detaches the same chunks, in the same order
as an uncapped one, spread over more ticks.

Only a server decides: a replica's step never solves or breaks (it replays the server's events, with
no new message on the wire). `DestructionWorld::load_report(id, top)` (and
`physics.destruction_load_report { id?, top? }`) solves on demand on any world, server or replica,
without changing anything; an asset without `load` is reported with the default settings and
`enabled: false`.

```json
{ "method": "physics.destruction_load_report", "params": { "id": 0, "top": 2 } }
→ { "instances": [{ "destructible": 0, "enabled": true, "iterations_run": 9, "over_limit": 0,
     "supports": [[0, 17658.0]], "top": [{ "bond": 0, "a": 0, "b": 1, "load": 17658.0, "max_load": 1.0e9, ... }],
     "problem": null, ... }], "stats": { "solves": 1, "non_finite": 0, ... }, "authority": "server" }
```

The model carries force, not bending moment: a cantilever fails at its root only because that bond
carries the whole overhang's weight. Zero masses are refused by the format; weights too large for
`f32` are refused when the asset loads (`destruction_invalid_asset`); cycles in the bond graph are
normal (walls, rings, arches). A solve that still overflows is dropped: every load is 0, no bond is
overloaded, `stats.non_finite` counts it and the report's `problem` is `destruction_load_not_finite`.

## Falling sections and free bodies (ADR-2734)

With `falling` settings (`.destructible.ron` v2), the chunks that leave the structure together and
are still bonded (at least `min_section_chunks` of them) fall as one dynamic body: one collider per
chunk on one rapier body, whose mass and centre of mass are the sum of the chunks'. Its velocity is
the chunks' momentum-weighted velocity. The section's bonds take damage from its own impacts (the
change of momentum between ticks that gravity does not explain, shared by its bonds); when one
breaks, the section splits into its bonded islands, each keeping the velocity of its centre of mass
(`v + ω × r`) and the section's spin, so linear momentum is kept. Single chunks become ordinary
debris bodies. A section counts as one body against `max_active_chunks`; past the cap single debris
goes back to the pool first, then the oldest section. Chunk entities, `debris_pose` and the draw
path work per chunk as before.

Which chunks form a section is decided from the break events, so a server and its replicas group the
same chunks; how a section moves and splits is debris motion, cosmetic and per peer.

`falling.free_body` (default on) makes an object with no support chunks a free dynamic body from
its first tick on a server: it falls, lands and can be knocked over, the world follows its pose (hits
land where it is), and its root entity's `Transform` follows it. A replica does not simulate it: it
follows the pose its game replicates, as for any moving structure. Without `falling`, nothing
changes: a collapsing section breaks into its chunks and a free object stays where it was placed
until it breaks.

`BridgeStats` reports `sections`, `splits_total` and `free_bodies`; a `BridgeReport` lists
`sections_spawned` and each `SectionSplit` (islands, momentum before and after).

## Impact damage (ADR-3272)

A car or a heavy crate that hits a structure breaks it once the structure is opted in. On the
server, after each physics step, `DestructionImpact` (inserted by `DestructionPlugin`, system
`convert_impacts` after `PhysicsStep`) reads the contacts on every intact chunk collider of the
enabled structures and turns each contact pair into at most one `Impulse` hit on the chunk under the
contact. The hit lands at the next destruction step, through the same model as any other hit: the
material's `damage_scale` and `min_impulse`, the break cap, the support graph. Clients never convert
contacts; they get the breaks from the server like any other (see
[Destruction over the network](destruction-net.md)).

```rust,wrap
# use std::sync::Arc;
# use regia_physics::destruction::{
#     Authority, BoxGrid, DestructibleAsset, DestructiblePose, DestructionMaterial, DestructionWorld, GridSupport,
# };
use regia_physics::destruction::{DestructionImpact, ImpactSettings};
# let mut world = DestructionWorld::new(Authority::Server);
# let grid = BoxGrid {
#     name: "wall".into(),
#     counts: [8, 4, 1],
#     cell: [0.5; 3],
#     density: 2400.0,
#     strength_per_area: 1000.0,
#     support: GridSupport::Bottom,
#     material: DestructionMaterial::default(),
# };
# let wall_id = world.define(Arc::new(DestructibleAsset::box_grid(&grid)?), DestructiblePose::default(), 1)?;
// In an app, DestructionPlugin inserts the resource: app.world_mut().resource_mut::<DestructionImpact>().
let mut impact = DestructionImpact::new(ImpactSettings { min_speed: 3.0, ..ImpactSettings::default() })?;
impact.enable(&world, wall_id)?; // contacts on this wall now hit it
// After each physics step (DestructionPlugin's convert_impacts does this):
// impact.convert(&physics, &destruction.physics, &mut destruction.world, dt);
```

For each pair the impulse is the contact impulse of the last step (the contact-force event's force
times the step), the point is where the contact pushed (impulse-weighted), and the contact speed is
the impulse over the pair's reduced mass: the speed the contact took out of the pair this step. A
pair becomes a hit only if it passes, in order:

| Gate | Default | Counter |
|---|---|---|
| The other side is not debris or another structure (`debris_hits` turns this off) | off: debris skipped | `debris_skipped` |
| impulse x `impulse_scale` x material `damage_scale` >= material `min_impulse` | material | `below_min_impulse` |
| contact speed >= `min_speed` (m/s) | 2 | `below_speed` |
| the pair (chunk collider, other collider) did not hit in the last `cooldown_ticks` | 10 | `cooling_down` |

At most `max_hits_per_tick` hits (default 32, over every structure) are queued per tick, strongest
first; the rest count in `dropped_by_cap`. A resting or pushing contact never passes the speed gate
(its contact speed is about `g x dt`, 0.16 m/s at 60 Hz). With debris hits off (the default) a falling
piece never damages the structure it fell from or another one, so breaks do not cascade.

**Damage radius.** A hit lands on the chunk whose collider the contact touched and loads only that
chunk's bonds. A crash therefore breaks loose only chunks the body touched, plus whatever the support
graph drops (the usual collapse rule). For the test crash (`cargo test -p regia_physics --test
destruction_impact`: the default car at 15 m/s into a 14 m wall of 0.5 m chunks) every broken chunk is
within the car's half-width plus one chunk of its centre line, and no chunk further than the documented
radius moves: the colliding body's bounding-sphere diameter plus one chunk's half-diagonal, measured
from the first contact point. The car still bounces on the tick it hits (the solver ran before the
break); the chunks come loose on the next tick. The solver may put most of a crash's impulse on one or two of
the touching chunks (in the test crash two chunks were hit, about 11500 and 5300 N s, and one broke
loose), so tune `impulse_scale`, the material's `damage_scale` and the bond strength together.

Agent:

```json
{ "method": "physics.destruction_impact_damage_set", "params": { "enable": [0], "min_speed": 3 } }
→ { "settings": { "min_speed": 3, "cooldown_ticks": 10, "max_hits_per_tick": 32, "debris_hits": false, "impulse_scale": 1 }, "enabled": [0], "authority": "server", "last": { ... }, "hits": [], "cooling_pairs": 0, "totals": { ... }, "changed": { "settings": true, "enabled": 1, "disabled": 0 } }
{ "method": "physics.destruction_impact_damage" }
→ { "settings": { ... }, "enabled": [0], "last": { "tick": 212, "pairs": 6, "hits": 4, "dropped_by_cap": 0, "below_min_impulse": 0, "below_speed": 2, "cooling_down": 0, "debris_skipped": 0, "rejected": 0, "replica": false }, "hits": [{ "id": 0, "chunk": 31, "impulse": 2210.5, "force": 132630, "speed": 7.4, "point": [0.2, 0.7, 0.25], "direction": [0, 0, -1], "debris": false, "queue": 3 }], ... }
```

`physics.destruction_impact_damage_set` takes the settings above (`min_speed`, `cooldown_ticks`,
`max_hits_per_tick`, `debris_hits`, `impulse_scale`; each optional, the rest keep their value) plus
`enable` and `disable`, arrays of instance ids to opt in or out; `changed` counts what moved. It checks
everything before it changes anything:
`destruction_impact_invalid` for a setting out of range (`min_speed` or `impulse_scale` negative or
not finite, `max_hits_per_tick` over 4096, `cooldown_ticks` over 36000), `destruction_unknown_id` for
an id in `enable` the world does not have. On a replica `last.replica` is true and nothing is read.
Nothing is read either while no structure is enabled, so a game that never opts in pays nothing.
`destruction_impact_convert` is a `tracing` span; the buffers are kept between ticks (a resting
contact allocates nothing).

## Determinism and networking

Every decision uses `f32` addition, multiplication, division and square root on plain arrays, in
fixed-step order. These round the same on every platform, so the same hits give the same breaks on
Linux, Windows, Android and macOS. The renderer and the GPU play no part. Debris motion after a
break is rapier's; it is cosmetic and never feeds back into a decision.

Each decision is a `BreakEvent { seq, tick, destructible, kind, seed }`: `BondBroken { bond, a, b,
position }` or `ChunkDetached { chunk, cause, position, linvel, angvel }`. Events are serde types. A
server (`Authority::Server`) decides them; a client built with `DestructionPlugin { authority:
Authority::Replica }` defines the same instances in the same order (ids are handed out in order)
and calls `destruction.world.apply_events(&events)`. Its `structure_hash()` then matches the
server's. A replica refuses damage (`destruction_replica`), and events must arrive in `seq` order
(`destruction_bad_event`). The `seed` of an event is the same on both sides; use it to pick sound or
particle variations. `regia_framework::destruction_net` carries all of this over `regia_net`,
with late joiners and resync: see [Destruction over the network](destruction-net.md).

`physics.destruction_state` reports `structure_hash` (chunk states, broken bonds and the digest of
every event: what a replica must match) and `state_hash` (adds the tick and every bond's
accumulated damage: what two servers fed the same hits must match).

## Caps, pooling, despawn

`DestructionBudget` (`physics.destruction_budget`):

| Field | Default | Meaning |
|---|---|---|
| `max_active_chunks` | 256 | Debris bodies alive at once. Spawning past it returns the oldest to the pool. 0: detached chunks get no body. |
| `max_breaks_per_tick` | 64 | Bonds broken per tick (0: unlimited). |
| `max_spawns_per_tick` | 32 | Debris bodies created per tick (0: unlimited); the rest wait in order. |
| `sleep_despawn_secs` | 4 | Debris asleep this long goes back to the pool (0: never). |
| `max_lifetime_secs` | 30 | Debris older than this goes back to the pool (0: never). |
| `kill_y` | -100 | Debris below this height goes back to the pool. |

Pooled bodies are disabled rapier bodies reused for the next chunk (`pool_reuses`); the pool never
holds more than the active cap. `destruction_step` and `destruction_physics_sync` are `tracing`
spans; `last_sync_us` in the budget report is the last sync's cost.

## Hooks: sound, VFX, navmesh, colliders

`DestructionPlugin` writes three messages each fixed tick:

- `DestructionBroke(BreakEvent)`: every decided or replayed break (send these to clients).
- `DestructionFxMessage(DestructionFx { kind, position, magnitude, fx_tag, seed })`: `bond_break`
  (crack, dust), `chunk_detach` (a piece knocked loose), `collapse` (rumble, falling debris). `fx_tag`
  is the material's tag (`"brick"`, `"wood"`). `regia_framework::destruction_fx::DestructionFxPlugin` plays
  these from a `.destructionfx.ron` table (sound cue, VFX system, decal per tag, with caps): see
  [Destruction effects](destruction-fx.md).
- `NavmeshInvalidated(NavInvalidation { destructible, min, max })`: one merged box per instance that
  lost chunks this tick. Rebuild the navmesh tiles under it.

When a chunk's debris body spawns, its collider leaves the intact structure in the same tick, so
raycasts and character controllers see the hole at once. Debris entities carry `DestructionChunk {
destructible, chunk }` and a `Transform` that follows the body; the intact structure is one entity
with `DestructibleRoot(id)`.

A whole game's worth of this over a network, with a fractured wall and tower, is in
[`samples/destruction_lab`](destruction-net.md#a-sample-to-copy).

## The sample: a wall and a bridge

`physics.destruction_define { "sample": "wall_bridge" }` places an 8 x 8 brick wall at z = -5 and a
12 x 2 plank bridge deck at y = 3, z = 5 whose end planks rest on abutments. The hit script
(`regia_physics::destruction::sample::sample_hits`) shoots one brick out of the wall on tick 10,
blasts the wall on tick 30, cuts the deck in two on tick 50 (each half still rests on its abutment),
knocks the planks next to the left abutment loose on tick 70 (the left half collapses), and blasts the
wall's lower left on tick 90. `cargo test -p regia_physics --test destruction` runs it and pins its
hashes; `cargo run -p regia_physics --example destruction_scene` prints them with the rapier side
running. `cargo run -p regia --example destruction --features quick` draws it (boxes for chunks,
debris following rapier, abutments and ground as fixed colliders) and restarts after the script;
with `REGIA_DESTRUCTION_CAPTURE=out.png` it saves the settled frame, prints the hashes and quits.
On lavapipe and on a GPU the hashes are the same: the renderer plays no part in a decision.

```json
{ "method": "physics.destruction_damage", "params": { "kind": "projectile", "origin": [0.1, 1.1, 5], "direction": [0, 0, -1], "impulse": 1500, "radius": 0.3 } }
→ { "queued": 0, "lands_on_tick": 1, "pending": 1 }
```

## Errors

Errors from the destruction API are `F0002` with `data.reason`:

| Reason | When |
|---|---|
| `destruction_parse` | `.destructible.ron` text that does not parse. |
| `destruction_version` | A `format_version` this engine cannot read. |
| `destruction_invalid_asset` | A rule of the format fails (bad grid counts, a bond with `a >= b`, a non-positive mass or threshold). |
| `destruction_unknown_id` | No instance has this id. |
| `destruction_unknown_chunk` | The instance has no such chunk. |
| `destruction_invalid_damage` | A hit parameter is out of range (radius 0, zero direction, non-finite values). |
| `destruction_invalid_pose` | A position or rotation that is not finite, or a zero rotation. |
| `destruction_replica` | Damage sent to a replica (only the server decides breaks). |
| `destruction_not_replica` | `apply_events` on a server world (Rust API only). |
| `destruction_bad_event` | A replayed event out of order or naming something the replica does not have (Rust API only). |
| `destruction_bad_snapshot` | A snapshot naming an asset, chunk or bond the replica cannot rebuild (Rust API only). |
| `destruction_impact_invalid` | An impact-damage setting out of range (`physics.destruction_impact_damage_set`). |

Reported, not raised: `destruction_load_not_finite` in a load report's `problem` when the load
solve overflowed (its loads are dropped).

## Limits

- The intact structure is a fixed body until it is moved: set its root entity's `Transform` (or call
  `DestructionWorld::set_pose` and `DestructionPhysics::follow_pose`) and it becomes a kinematic body
  that follows the pose, so a structure can ride a vehicle or a ship's hull. Debris of a moving
  structure starts with the structure's velocity at its point plus the decided detach velocity. A free
  object (no support chunks) is a free rigid body only with `falling.free_body`, and only on the
  server; without it, it does not fall until it breaks.
- A collapsing section falls as one piece only with `falling` settings; without them it breaks into
  its chunks. A section splits on its own impacts per peer (cosmetic); it never feeds a break decision.
- Gravity loads bonds only with `load` settings, and the model carries weight, not bending moment or
  torsion. Without them a structure still connected to a support never fails under its own load.
- Loads are solved on the structure as built; a falling section's or a free body's own weight does not
  load its bonds.
- Debris hitting another destructible does no damage unless impact damage is on for the struck
  structure with `debris_hits` (see "Impact damage"). Impact damage reads rapier's contact impulses,
  so it is as deterministic as the rapier step on one platform; clients never convert contacts.
