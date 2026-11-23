# Fog of war

Each team sees what its own and allied units, buildings and long-sight structures can see. Regia
computes that as a pure function of the sim state (`regia_detsim::fog`), and the sim bridge can show
one viewing team only what it sees (`regia_framework::fog_view`). The sim itself keeps stepping the
full state, so lockstep peers never diverge because two players see different things (plan row
S-16, ADR-8154).

## The rules

For a viewing team `v` and an entity of team `t`:

| Case | Shown to `v`? |
|---|---|
| Fog off (`FogRules::enabled` false) | yes |
| `v` is an observer: team 0, or above `FogRules::team_count` (default 4) | yes, everything |
| `t` is neutral (0 or above the team count), `t == v`, or `t` is allied to `v` | yes |
| The entity is dead (`alive: false`) | yes |
| The entity is a map feature (`SightClass::Feature`) | yes |
| An enemy unit, unit on foot or building | only while a living own or allied watcher has it inside its sight radius on the ground plane (X and Z) and the line of sight is clear |

Sight radii are data, a `SightTable`: a default per class (unit 150 m, on foot 60 m, building
100 m, features never watch) and kind rows that override by the snapshot's kind key, for example a
radar mast at 400 m or a decoy at 0 m (a 0 radius never watches):

```rust,wrap
// The same types as `regia_detsim::fog`; `fog_view` re-exports them.
use regia_framework::fog_view::{FogRules, SightTable};

let rules = FogRules {
    team_count: 6,
    sight: SightTable::default().with_kind("radar_mast", 400.0).with_kind("decoy", 0.0),
    ..FogRules::default()
};
assert!(rules.sight.first_invalid().is_none());
```

`SightTable::first_invalid` names a negative, infinite or NaN radius; check it when the radii come
from a settings file.

## Line of sight

`regia_detsim::los::LineOfSight` is the hook: `clear(from, to) -> bool`. The default, `OpenGround`,
never blocks. A host plugs in its terrain or world-geometry query; a closure works too. The hook is
asked only for watchers already in range, so a costly ray test runs only where it matters. AI
perception uses the same trait, so one geometry query serves both.

## The pure functions

The sim's state (or a snapshot of it) implements `FogWorld`: one `FogEntity` row per entity (id,
kind, `SightClass`, team, position, alive) and `is_allied(a, b)`.

| Function | Answers |
|---|---|
| `entity_visible_to_team(world, rules, viewer, entity, los)` | is this entity shown to the viewer |
| `team_sees_point(world, rules, viewer, pos, los)` | does any watcher of the viewer see this point |
| `TeamVision::new(world, rules, viewer)` / `rebuild` | the viewer's watchers and allies, gathered once; `entity_visible` and `sees_point` answer as the two functions above |
| `visible_ids(world, rules, viewer, los, &mut vision, &mut out)` | the sorted ids the viewer sees, and how many were hidden |
| `FogGrid` | a cell grid for a minimap and a fog overlay: the explored layer `seen` and the visible-now layer `visible` (32 m cells by default; `try_covering` takes a cell size; `update(&vision, los)` rebuilds the visible layer; `stamp_team` marks a team's watchers as explored) |
| `team_detects_point(world, rules, viewer, pos, los)` | does any detector of the viewer detect this point (see "Stealth and detectors") |
| `FogMemory::update(world, &vision, los, tick)` | the last-seen ghosts of remembered structures, per viewing team |

The arithmetic is IEEE `+ - *` in a fixed order (no transcendentals, no hash iteration), so the
answers are the same on every peer and architecture; `cargo xtask det-scan` checks the module. Do not
feed visibility back into gameplay that runs in lockstep unless every peer computes it for every
team.

## Explored, visible and ghosts

A fog cell has three states, `CellState::{Unexplored, Explored, Visible}`. `FogGrid` keeps two
layers per viewing team: `seen` (explored: a watcher saw the cell at some point) and `visible`
(a watcher sees it now). `FogGrid::update(&vision, los)` clears `visible`, marks every cell whose
center lies inside a watcher's radius with a clear line from the watcher's eye, and adds those cells
to `seen`; `state_at(x, z)` and `state(col, row)` read a cell. With `OpenGround` the visible layer is
exactly the radius stamp of the watchers. A cell has no height, so the hook is asked for the cell
center at `FOG_CELL_POINT_Y` (`f32::MIN`); a terrain hook lifts it onto its ground. `update` does not
apply the elevation rule to cells; `FogGrid::update_on_ground(&vision, los, |x, z| height)` does (KI-GP-018,
ADR-10070): a cell whose ground stands more than `step_m` above a watcher is seen only inside that
watcher's scaled radius. Without an elevation rule both give the same cells. On a `FogViewSim`,
`with_cell_ground(|x, z| height)` makes its grid use it.

A `FogViewSim` grid holds the viewer's explored ground; by default switching the viewer starts an
unexplored grid. `with_explored_per_team()` keeps one grid per player team, updated on every snapshot
(one vision rebuild and grid update per team), so a switch shows the new team's explored ground at once
and keeps the old team's; `team_grid(team)` reads any of them (KI-GP-018, ADR-10070).

Structures stay on an RTS map after they leave sight: mark their kind `remember` in the sight table
and keep a `FogMemory`:

```rust,wrap
use regia_framework::fog_view::{FogRules, KindSight};

let mut rules = FogRules::default();
rules.sight = rules.sight.with_row(KindSight::new("depot", 100.0).remembered());
```

Each `FogMemory::update` for a viewer refreshes the ghost of every remembered enemy it sees now
(id, kind, class, team, position and the tick), drops the ghost of one it sees dead, and drops every
other ghost whose spot a watcher now sees (the spot is seen empty: the structure moved, died unseen,
or is stealthed with no detector near). A ghost holds only what was seen; nothing about the live
entity reaches it while it is hidden. Ghosts are per viewing team and sorted by id; observers and
fog off hold none.

`FogViewSim` keeps a `FogMemory` and adds a `ViewerGhost` row (id, kind, team, translation, tick)
to `ViewerSnapshot::ghosts()` for every remembered structure the viewer does not see now. Ghosts are
not spawned as sim entities; a fog presentation draws them. `clear_memory()` forgets them (a new
match).

## Terrain and elevation

Two separate tools:

- **Terrain occlusion** is a line-of-sight hook. `regia_nav::det::fog_los::GridLineOfSight` reads
  the det grid's ground heights (`NavStatic`): both ends snap to their nearest cell, each end's
  height is the higher of its `y` and its ground (plus `eye_m` at the watcher and `target_m` at the
  target), and the ray walks every cell it touches between them (the grid's supercover walk). At each
  cell between the two ends the ray height is interpolated at the cell center's projection onto the
  segment; ground above the ray blocks. Integer offsets and IEEE `+ - * /` only, so it is lockstep
  safe.
- **Eye heights** per sight class (`SightTable::{unit_eye_m, on_foot_eye_m, building_eye_m}`, 0 by
  default) raise the watcher's point that the hook is asked from: a watchtower sees over a ridge a
  tank behind it cannot.
- **The elevation rule** (`FogRules::elevation: Option<ElevationRule>`, off by default) is a radius
  rule: when a target stands more than `step_m` above the watcher (`target.y - watcher.y > step_m`,
  the reported positions), the watcher's radius toward it is `radius * low_ground_scale` (clamped to
  `0..=1`; NaN reads as 0). A scale of 0 is the classic "low ground never sees high ground".

<!-- ignore-reason: a sketch; `nav_static` is the game's NavStatic and `my_sim` its ExternalSim -->
```rust,ignore
use std::sync::Arc;
use regia_framework::fog_view::{ElevationRule, FogRules, FogViewSim};
use regia_nav::det::fog_los::GridLineOfSight;

let mut rules = FogRules::default();
rules.sight.unit_eye_m = 1.8;
rules.elevation = Some(ElevationRule { step_m: 4.0, low_ground_scale: 0.0 });
let terrain = GridLineOfSight::new(Arc::clone(&nav_static)).with_target_m(1.0);
let view = FogViewSim::new(my_sim, rules, 1).with_line_of_sight("det_grid", terrain);
```

## Stealth and detectors

A kind row can be stealthed (`KindSight::stealthed()`) or a detector (`KindSight::detector(m)`).
A living stealthed enemy shows only while a living own or allied detector has it inside its
detection radius (XZ) with a clear line of sight from the detector's eye; ordinary sight does not
reveal it. Otherwise it is filtered exactly like a hidden unit: no entity, no extras, no events on
it. The dead always show. A detector with radius 0 (`radius_m` 0) detects without watching.

```rust,wrap
use regia_framework::fog_view::{FogRules, KindSight};

let mut rules = FogRules::default();
rules.sight = rules
    .sight
    .with_row(KindSight::new("cloak", 150.0).stealthed())
    .with_row(KindSight::new("detector_van", 150.0).detector(80.0));
assert!(rules.sight.first_invalid().is_none());
```

## Cost at scale

`TeamVision` gathers a viewer's watchers and detectors once per rebuild. With 16 watchers or more
(`DEFAULT_INDEX_MIN_WATCHERS`; `set_index_min_watchers` changes it) it files them into a
`regia_detsim::spatial::DetSpatial` uniform grid, each row's disc its sight radius, and a point
query tests only the watchers whose disc can hold the point, stopping at the first that sees. The
accepted set is the linear scan's (the oracle test runs both over 24 seeded worlds x 7 viewers and
900-entity worlds). Long-sight watchers (more than twice the median radius, such as radar masts)
stay on a short list every query tests, so one mast does not widen every query.

The sight table is compiled once per change (`SightTable::compile()` -> `CompiledSight`): an entity
that carries `FogEntity::kind_id` (from `CompiledSight::kind_id`) is looked up by index, one without
by a binary search over the kind names. Line of sight is asked only for watchers in range.

`TeamVision::{sees_point_counted, entity_visible_counted}` add their work to a `FogCounters`
(watchers, detectors, queries, index candidates, line-of-sight calls), and `fog.status` reports the
last frame's. No wall clock: a profile reads the `fog_view_filter` tracing span. The measured cost
(900 entities, 4 teams) is in this lane's evidence file, not a test.

`FogViewSim` filters a local view. A server-authoritative game that must not send hidden entities
uses the fog relevance filter on the server (see "Fog over the network" below).

## Showing one viewer only what it sees

Wrap the sim in `FogViewSim` and give it to the bridge; add `FogViewPlugin` for the agent methods:

<!-- ignore-reason: a sketch; `MySim` is the game's ExternalSim whose snapshot implements FogWorld -->
```rust,ignore
use regia_framework::fog_view::{FogRules, FogViewPlugin, FogViewSim};
use regia_framework::sim_bridge::SimBridgePlugin;

app.add_plugins((
    SimBridgePlugin::new(
        FogViewSim::new(MySim::new(), FogRules::default(), 1).with_line_of_sight("terrain", my_terrain_los),
    ),
    FogViewPlugin::<MySim>::default(),
));
```

Each frame `FogViewSim` reads the full snapshot, works out the viewer's visible ids and hands the
bridge a `ViewerSnapshot` holding owned copies of only the visible rows and their extras. A hidden
unit never becomes an entity, a transform or `SimExtras` in the viewer's world; it spawns when a
watcher comes in range and despawns when it leaves sight. A snapshot row with no fog row is held back
(fail closed) and counted as `unlisted`.

Events: with `FogEventPolicy::HideUnseen` (the default) an event placed on an entity the viewer does
not see this tick, or at a point outside its sight, is dropped; unplaced events pass. `PassAll` keeps
every event. Report a destroyed entity as dead (not removed) on the tick it dies: the dead always
show, so its death events reach every viewer.

A dedicated server decides what each client is sent with the fog relevance filter (next section).

## Fog over the network

`FogViewSim` hides enemies from one local view. In a server-authoritative or listen-server game
that is not enough: the replication layer would still send every hidden enemy to every client, and
a modified client could draw them. `regia_framework::fog_relevance` (feature `net`) connects the fog
rules to the net relevance pipeline ([Net relevance](net-relevance.md), spec 72 § 5.1 stage 6): a
custom relevance filter named `fog` removes, per connection, the enemies that connection's team
cannot see. A removed entity is never replicated to that client: no spawn, no component, no
position.

<!-- ignore-reason: a sketch; `app` is the game's server app with NetPlugin and install_net_interest -->
```rust,ignore
use regia_framework::fog_relevance::{
    FOG_RELEVANCE_PROFILE, FogRelevancePlugin, FogRelevanceSettings, FogTeamOf, FogUnit,
};
use regia_framework::fog_view::{FogRules, SightClass};
use regia_net::{NetProfileRef, PeerId};

app.add_plugins(FogRelevancePlugin::new(FogRules::default(), FogRelevanceSettings::default()));
// The game puts each connection on a fog team (from its lobby).
app.world_mut().resource_mut::<FogTeamOf>().set(PeerId(1), 1);
// A fogged unit: a FogUnit and a profile that names the `fog` filter.
app.world_mut().spawn((
    Replicated::default(),
    Transform::default(),
    FogUnit::new(2, "tank", SightClass::Unit),
    NetProfileRef(FOG_RELEVANCE_PROFILE.into()),
));
```

The pieces:

| Piece | What it does |
|---|---|
| `FogUnit { team, kind, class, alive }` | On a server entity fog may hide. Its position is its world translation (`GlobalTransform` for a child, else `Transform`); an entity with no position is hidden from every enemy team. |
| `FogTeamOf` | `PeerId -> TeamId`, filled by the game. Team 0 (or above `team_count`) observes everything. A connection with no team is refused every fogged entity (fail closed), its own entities excepted: the net owner rule runs before the filter. |
| `FOG_RELEVANCE_PROFILE` (`"fog"`) | A built-in replication profile `install_fog_relevance` adds when the world has `NetProfiles`: world relevance (fog decides, not distance), `custom_filter: "fog"`, `linger_ticks: 0`. A project profile can name the filter itself (`custom_filter: Some("fog")`); give it `linger_ticks: 0`, or the profile's linger adds to the fog's. |
| `fog_relevance_prepare` | Once per server tick, before the send (`FogRelevancePlugin` puts it in `FixedUpdate`, `NetSet::Gather`, servers only): one `TeamVision` per mapped player team, shared by every connection of that team, plus one lead vision per team while the lead is on. The filter is then a set lookup, so the per-connection cost does not grow with the watcher count. |
| `FogEffect { source }` | On a projectile or effect entity (see "Events" below). |
| `fog_event_shown(world, peer, place)` | Whether the server may send a message placed at an `EventPlace` to `peer`. |

Every visibility rule is the one above (allies share sight, neutral, dead and map features always
show, stealth needs a detector, the line-of-sight hook gates every watcher); `FogRelevance::new(rules,
settings).with_line_of_sight(name, los)` plugs in the same hook as the view.

Grace, in server ticks (`FogRelevanceSettings`):

- **Linger** (`linger_ticks`, default 6): an enemy that leaves sight stays relevant for that many
  more ticks, so the client's interpolation does not pop it out mid-motion.
- **Lead** (`lead_ticks`, default 4, with `max_step_m`, default 0.5 m): an enemy becomes relevant up
  to `lead_ticks` ticks before it enters sight. The test is a second vision whose sight and
  detection radii are widened by `lead_ticks x max_step_m` metres (`fog_view::widened_sight`): set
  `max_step_m` to the farthest any fogged entity moves in one tick, and an enemy outside that ring
  cannot enter sight within `lead_ticks` ticks. The lead vision asks the same line-of-sight hook, so
  it never reveals through walls; an enemy that steps around a corner can still appear on the tick
  it is seen. The lead is the price of smooth arrivals: an enemy inside the ring is on the client
  (not drawn by a fog view, but present) a few ticks early.

Events: an entity carrying `FogEffect` is shown to a team under `FogEventPolicy::HideUnseen` (the
default, `settings.events`) while its `source` is shown to that team or its own position is in that
team's sight; a shot fired by a hidden enemy stays hidden until it flies into sight. `PassAll`
shows every effect. A message the server sends itself (a hit marker, a sound cue) asks
`fog_event_shown(world, peer, place)` first: `EventPlace::Entity` carries the server entity's
`Entity::to_bits()`, `At(p)` asks the team's sight, `Unplaced` passes.

Lockstep: in a lockstep game every peer holds the full state by design, so fog there is
presentation only and a modified client can read every unit. This filter does not apply. A
competitive lockstep game needs server validation of what each peer may know, which is out of
scope. A listen-server host holds the full state too (it is the server): hide enemies from its local
view with `FogViewSim` or the game's own presentation filter.

## Showing the fog

Give the view a grid and add the presentation plugin; install the world overlay on the renderer:

<!-- ignore-reason: a sketch; `MySim` is the game's ExternalSim and `renderer` its FrameRenderer -->
```rust,ignore
use regia_framework::fog_texture::FogPresentationPlugin;
use regia_framework::fog_view::{FogGrid, FogRules, FogViewPlugin, FogViewSim};
use regia_render::{FogLook, FogOverlayHandle};

let overlay = FogOverlayHandle::default();
renderer.enable_fog_overlay(&overlay); // registers the `fog_of_war_overlay` pass (before_post)
app.insert_resource(overlay)
    .insert_resource(FogLook::on())
    .add_plugins((
        SimBridgePlugin::new(
            FogViewSim::new(MySim::new(), FogRules::default(), 1).with_grid(FogGrid::covering(1024.0, 1024.0)),
        ),
        FogViewPlugin::<MySim>::default(),
        FogPresentationPlugin::<MySim>::default(),
    ));
```

Every filtered snapshot updates the grid for the viewer (explored and visible cells, through the
same line-of-sight hook). `FogPresentationPlugin` then syncs the **fog texture**
(`regia_framework::fog_texture::FogTexture`): one R8 texel per cell, 0 unexplored, 128 explored,
255 visible. Two rules keep it cheap and smooth:

- **Only dirty rows upload.** The grid keeps a dirty-row bitset (`FogGrid::dirty_rows`: rows whose
  cell states changed); a sync reads only those rows and the rows still fading, and hands the
  overlay the rows whose bytes changed, as contiguous runs (one texture copy each). A view that does
  not change uploads nothing; a grid of another size uploads once, whole.
- **Cells fade, frame-rate independently.** The shown value moves toward its target as
  `shown += (target - shown) * (1 - exp(-dt / fade_s))` (default `fade_s` 0.12 s), so 30, 60 and
  120 Hz reach the same value at the same time; a value within half a step snaps to its target.

The **world overlay** (`regia_render::fog_overlay::FogOverlayPass`, slot `before_post`) reconstructs
each pixel's world position from the scene depth, samples the texture at world X/Z with a 3 x 3
tent filter (tap spacing `FogLook::softness` cells: soft edges) and multiplies the HDR image by a
colour between the unexplored look, the explored look and white (visible). The sky is left alone;
ground outside the grid reads as unexplored. `FogLook` (a resource) holds `enabled` (off by
default), `strength`, `explored_darkness`, `explored_tint`, `unexplored_darkness`,
`unexplored_tint` and `softness`. With the look off, or before the first grid, the pass records
nothing and the frame is unchanged.

To verify a change, read `fog.texture_stats`, turn the overlay on with `fog.set_look`, and take a
viewport screenshot (`editor.screenshot`, or `seeing.screenshot` in a game): unexplored
ground is dark, explored ground dim and tinted, visible ground unchanged.

## Minimap

A `regia_ui` radar or minimap shows the fog through a `RadarFog` component on its node (a runtime
binding: `.ui.ron` is unchanged):

<!-- ignore-reason: a sketch; `minimap` is a Radar node the game spawned -->
```rust,ignore
use regia_ui::game_widgets::RadarFog;

commands.entity(minimap).insert(RadarFog::new(0xF06_0001)); // a renderer image id the game picks
```

The presentation plugin keeps the layer's `world_rect` equal to the grid's and re-queues the image
(`radar_fog_rgba`: unexplored `RadarFogLook::unexplored`, explored `RadarFogLook::explored`, visible
clear; row 0 is north) whenever rows changed. The layer draws over the map and under the blips, as a
window of its rect like the map image.

## Debug view

The editor's viewport toolbar has **Show ▸ Fog of war** (`editor.set_viewport { fog: true }`,
applied to the play world too). While it is on, the game's `FogDebugView` is filled every frame and
the viewport draws each friendly watcher's sight circle and the grid's outline; the Show menu shows
the fog texture (black unexplored, grey explored, white visible), its size and last upload, and a
**Viewer team** picker that queues `fog.set_viewer` (0 observes everything).

## Agent methods

### `fog.status`

```json
fog.status {}
→ { "view": true, "enabled": true, "viewer": 1, "observer": false, "team_count": 4, "sight_kinds": 2,
    "line_of_sight": "open_ground", "event_policy": "hide_unseen", "frames": 240, "tick": 240,
    "visible": 2, "hidden": 1, "unlisted": 0, "watchers": 1, "events_passed": 2,
    "events_dropped": 2, "detectors": 0, "queries": 1, "index_candidates": 1, "los_calls": 1,
    "ghosts": 0, "ghosts_dropped": 0, "pending": { "viewer": null, "enabled": null }, "net": null }
```

`visible`, `hidden`, `unlisted`, `watchers` and `detectors` are the last filtered frame's;
`queries` (entities and ghost spots answered from sight), `index_candidates` (watchers tested
exactly), `los_calls` (line-of-sight hook calls), `ghosts` (remembered structures the viewer holds
a ghost of and does not see now) and `ghosts_dropped` are the last frame's work counters; `events_*`
are running totals; `pending` is a `fog.set_viewer` request not yet applied. The status reports
counts only, never a hidden entity's id, kind or position, and never a ghost's row. To check that a unit is hidden, read the scene instead:
`scene.query_entities` with the `SimId` component finds no entity for it.

`net` is the server's fog relevance, `null` without one. A server with only the fog relevance (no
fog view) answers `{ "view": false, "net": { ... } }`:

```json
fog.status {}
→ { "view": false, "net": { "enabled": true, "team_count": 4, "linger_ticks": 6, "lead_ticks": 4,
    "lead_margin_m": 2.0, "event_policy": "hide_unseen", "line_of_sight": "open_ground",
    "ticks": 600, "units": 12, "unplaced": 0, "effects": 0, "visions_built": 2,
    "lead_visions_built": 2, "visions_total": 1200,
    "teams": [ { "team": 1, "peers": 4, "shown": 7, "hidden": 5, "linger_hits": 1, "lead_hits": 0,
                 "watchers": 6, "effects_shown": 0, "effects_hidden": 0, "queries": 12 } ],
    "peers": [ { "peer": 1, "team": 1, "evaluated": 3600, "filtered": 3000 } ],
    "unmapped_refusals": 0, "unlisted_refusals": 0, "linger_hits_total": 40,
    "lead_hits_total": 12, "events_passed": 0, "events_dropped": 0 } }
```

`visions_built` and `lead_visions_built` are the last tick's (one per mapped player team, never one
per connection); `teams[]` is the last tick's; `peers[]` (`evaluated`: fogged entities the filter
was asked about for that connection, `filtered`: of those, refused) and the `*_total`,
`unmapped_refusals` (connections with no team), `unlisted_refusals` (entities on the fog profile
with neither `FogUnit` nor `FogEffect`) and `events_*` (`fog_event_shown` answers) counters are
running totals. Counts only: never a hidden entity's id, kind or position. To check that a client
does not hold an enemy, read the client's scene, or `net.relevance` on the server for that peer.

### `fog.set_viewer`

```json
fog.set_viewer { "viewer": 2 }
→ { "before": { "viewer": 1, "enabled": true }, "after": { "viewer": 2, "enabled": true },
    "applied": true, "note": "applies at the start of the next frame; fog.status then reports it" }
```

`viewer` switches the viewing team (0 observes everything), `enabled` turns fog on or off; give one
or both. `dry_run: true` returns `before` and `after` and changes nothing. Needs the `play`
permission. The request applies at the start of the next frame: `wait_until` `fog.status` reports
the new viewer, then read the scene.

### `fog.texture_stats`

```json
fog.texture_stats {}
→ { "cols": 40, "rows": 40, "origin": [-640.0, -640.0], "cell_m": 32.0, "dirty_rows": 6,
    "fading_rows": 6, "rows_uploaded": 6, "upload_bytes": 240, "runs": 2, "syncs": 120,
    "total_upload_bytes": 4080, "full_uploads": 1,
    "overlay": { "cols": 40, "rows": 40, "draws": 119, "texture_creates": 1, "rows_uploaded": 64,
                 "bytes_uploaded": 2560, "last_rows": 6, "copies": 3, "failures": 0, "pending_rows": 0 },
    "look": { "enabled": true, "strength": 1.0, "explored_darkness": 0.55,
              "explored_tint": [0.8, 0.85, 1.0], "unexplored_darkness": 0.92,
              "unexplored_tint": [1.0, 1.0, 1.0], "softness": 0.75 },
    "debug_view": false }
```

The texture's size and place, and the last sync's `dirty_rows` (grid rows whose states changed),
`fading_rows`, `rows_uploaded`, `upload_bytes` and `runs`; `syncs`, `total_upload_bytes` and
`full_uploads` are running totals. `overlay` is the GPU pass's counters (`null` without a
`FogOverlayHandle` resource; `pending_rows` waits for the next frame), `look` the `FogLook` resource
(`null` when the app has none), `debug_view` whether the editor's Show ▸ Fog of war is on. A steady
view reads `rows_uploaded: 0`.

### `fog.set_look`

```json
fog.set_look { "enabled": true, "softness": 1.5 }
→ { "before": { "enabled": false, ... }, "after": { "enabled": true, "softness": 1.5, ... },
    "applied": true, "note": "..." }
```

Sets `enabled`, `strength` (0 to 1), `explored_darkness` (0 to 1), `unexplored_darkness` (0 to 1)
and `softness` (0 to 2) of the `FogLook` resource from the next frame; omitted fields stay.
`dry_run: true` returns `before` and `after` and changes nothing. Needs the `play` permission. The
overlay draws only when the app registered it (`FrameRenderer::enable_fog_overlay`).

### Errors

| Code | When |
|---|---|
| `F0002` | `fog.set_viewer` with neither `viewer` nor `enabled`, a `viewer` that is not a team number from 0 to 4294967295, or an `enabled` that is not a boolean |
| `F0003` | `fog.status`: neither a fog view nor a fog relevance in the app (wrap the sim in `FogViewSim` and add `FogViewPlugin`, or add `FogRelevancePlugin` on a server); `fog.set_viewer`: no fog view |
| `F0002` | `fog.set_viewer` with neither `viewer` nor `enabled`, a `viewer` that is not a team number from 0 to 4294967295, or an `enabled` that is not a boolean; `fog.set_look` with a number out of its range or not a number, or an `enabled` that is not a boolean |
| `F0003` | no fog view in the app: wrap the sim in `FogViewSim` and add `FogViewPlugin`; for `fog.texture_stats` and `fog.set_look`, no fog presentation: give the view a grid (`with_grid`) and add `FogPresentationPlugin` |

## Moving a game's fog onto Regia

A game with its own fog implements `FogWorld` for its state (rows from its entity list,
`alive = !destroyed`, `SightClass::OnFoot` for crew out of a vehicle, `is_allied` from its own
alliance table) and builds a `FogRules` with its long-sight structures as kind rows. With
`OpenGround` the answers equal the radius test it had before, so its fog tests keep their results.
