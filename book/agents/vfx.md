# VFX

`.vfx.ron` v7 is the particle asset (`docs/formats/vfx-ron.md`). v1 through v6 load and
migrate in memory. v3 adds `FireAnim`; v4 adds the `FireballAnim` body sheet and `heat_haze`;
v5 adds the `Planar` velocity mode (outward along the ground when local +Z is the ground normal);
v6 adds the scene wind and event spawns (below); v7 adds ribbons and mesh shapes.
Crash effects (fireball, dust ring, spray sheet, smoke column) are on the
[crash effects](crash-fx.md) page.

## Author a system

```ron,vfx
(
    version: 7,
    name: "smoke",
    system: (budget: 1024),
    emitters: [
        (
            name: "puff",
            delay: 0.0,
            spawn: (mode: Rate(12.0), lifetime: (min: 1.0, max: 2.0)),
            init: (
                shape: Point,
                size: (min: 0.4, max: 0.8),
                color: [(t: 0.0, color: "#9a9288")],
            ),
            render: (
                mode: Billboard,
                sprite: PuffAnim,
                blend: Alpha,
                softness: 1.2,
                lit: true,
                emissive: 1.0,
                self_shadow: 0.7,
                screen_floor_px: 2.0,
                flipbook: Some((
                    cols: 8,
                    rows: 2,
                    frames: 16,
                    mode: Fps(10.0),
                    random_start: true,
                    blend_frames: true,
                )),
            ),
        ),
    ],
)
```

Fire stays in the orange/yellow/white family in linear light, before AgX. An HDR
`emissive` above 1 feeds bloom on a single-sample `Rgba16Float` frame (ADR-049). The
present pass adds that bloom and scales the peak. It does not repaint hue (ADR-049). A
pink core fails the hue test. Desktop High is exposure, `mix(base, bloom, 0.04)`, AgX
and `display_look(1.2, 1.4)`. Alpha emitters sort back to front; additive emitters do
not. Smoke is lit alpha puffs, not an additive disc. The muzzle flash uses the flare
sprite. The explosion is a flash, one fire body from the `FireballAnim` sheet (hot core, sooty rim,
flame tongues on top), its light on the ground, a heat-haze card, ember streaks, then a lit
smoke column.

## Check it

`samples/vfx_lab --evidence <dir>` writes the gallery. The GPU tests in
`crates/regia_vfx/tests/quality.rs` reject a hard ground cut, an unlit puff, a popping sort,
a pink core, a frozen flipbook and a sub-pixel spark. Plates live in
`docs/evidence/vfx-quality/`.

## Systems: list, describe, spawn, change

These work on any `.vfx.ron` system, whatever its effect.

- `vfx.systems` lists the loaded systems and every live instance: `live`, `active`, `age`, `origin`,
  `forward`, `scale`, `intensity`, `lod_scale` and `gpu_ms_estimate` (the live count at the 2 ms per
  100k budget rate of spec 26 § 6: a planning number, not a measurement). `wind` is what particles
  drift in. With `"gpu": true`, `gpu` holds the pool's counters read back from the device (it
  blocks on the GPU): `alive`, `dropped`, `events_fired`, `events_overflow`, `children_spawned`,
  `children_dropped` and the two capacities; it stays `null` without a GPU context. `live` counts
  scheduled particles; event children are only in `gpu.alive`.
- `vfx.describe` reads one system: a line per emitter (`spawn_mode`, `lifetime`, `wind_response`,
  `wind_drag`, `drag`, `events`, `event_tail_s`, and since `.vfx.ron` v7 `shape`, `kill` and
  `render`) and the whole `definition`. `shape.kind` is `point`, `sphere`, `box`, `cone`,
  `mesh_surface` or `skinned_mesh`; a mesh shape adds `mesh` and `mesh_status` (`ok` with
  `triangles`, `area` and `skinned`, or `missing_mesh` / `bad_mesh` with `error`). `kill` holds
  `below` (the `kill_below` plane), `volume` (`shape`, `half_extents` or `radius`, `center`, `kill`:
  `inside` or `outside`) and `depth_collision`. `render.mode` is `billboard`, `stretched`, `mesh`
  or `ribbon`; a ribbon adds `link`, `max_points` and `texture`. A mesh particle (`.vfx.ron` v8)
  adds `mesh` and `mesh_status`: `cube` (no `mesh`, the built-in cube, `indices` 36), `ok` (the
  registered mesh it draws, with `triangles` and `indices`), or `missing_mesh` / `bad_mesh` with
  `error` (it draws nothing), and `lit` (the mesh asset is shaded by the sun and ambient; the cube
  is always `false`). `mesh_draws` lists the GPU's mesh-asset draws of the last frame, one per
  registered render mesh: `mesh`, `slot`, `indices` and `instances` (that mesh's live particles,
  nothing else); it is `null` without a GPU context and empty with no registered mesh.
- `vfx.spawn_probe` plays a loaded `system` for `frames` frames (1..=600, default 1) with `seed`
  at `origin` along `forward` on a scratch instance and returns, per emitter, `spawned`, `refused`
  (spawns a mesh shape refused), `mesh_error` and the first `max` (1..=4096, default 256)
  `positions` with their `velocity`. Nothing is spawned; the same seed always gives the same
  positions, so it checks where a mesh shape puts particles before playing it.
- `vfx.spawn` plays a loaded `system`, or a project-relative `asset`, which it loads first (a loaded
  definition of the same name is replaced), at `origin` along `forward`, with optional `seed`,
  `scale` and `intensity`. It returns the instance `id` and what it `plays` in the level's medium.
- `vfx.set_param` changes a live `instance` (`scale`, `intensity`, `origin`, `forward`) or a loaded
  `system` + `emitter` (`wind_response`, `wind_drag`, `drag`, `count_scale`, `alpha_scale`,
  `emissive`; running instances follow on their next update). It returns `{ param, before, after }`.
  Changing an emitter validates the definition first, so a refused value changes nothing.

```json
{"method": "vfx.spawn", "params": {"asset": "fx/shell_impact.vfx.ron", "origin": [4.0, 1.0, 9.0]}}
{"method": "vfx.set_param", "params": {"system": "shell_impact", "emitter": "dust", "param": "wind_response", "value": 0.5}}
{"method": "vfx.systems", "params": {"gpu": true}}
```

Refusals are `F0002` with `data.reason`:

| `data.reason` | Meaning |
|---|---|
| `missing_param` | A required parameter is absent. |
| `bad_source` | Not exactly one of `asset` and `system`, or of `instance` and `system`. |
| `path_escapes` | `asset` is absolute, holds `..`, or a link leads out of the project. |
| `asset_invalid` | `asset` does not parse or validate; the message says why. |
| `unknown_param` | `vfx.set_param` has no parameter of that name. |
| `value_out_of_range` | A value is not finite or fails the emitter's rules. |

A system, emitter, instance or file that does not exist is `F0003`; a world without `VfxPlugin` is
`F0016`.

## Wind and event spawns

An emitter with `wind_response` above 0 drifts in the scene wind (`render.set_foliage_wind`, the
weather or a host's `SceneWind`): the wind's velocity pulls it at `wind_response * wind_drag` per
second, so without other forces it ends up moving with the wind, and still-air `drag` holds it to
a fraction. Change it live: `vfx.set_param` `wind_response`, then watch `vfx.systems`
`wind` and a screenshot. The default of 0 keeps every older effect exactly as it was.

`events` spawn a child emitter (spawn mode `Event`) from a particle's `Spawn`, `Death` or `Collision`
(the depth-collision path) with `count`, `probability` and `inherit_velocity`: sparks when a shell
lands, dust when it dies. See the example and the rules in `docs/formats/vfx-ron.md`. Counts are
deterministic: a replay fires the same events.

To check one: `vfx.spawn` it, `wait_until` the parent's lifetime has passed, then `vfx.systems`
with `gpu: true`. `children_spawned` is the parents fired times `count` at probability 1. A rising
`events_overflow` or `children_dropped` means the frame asks for more than
`VfxProfile::event_capacity` or `child_capacity`: lower the rates or raise the profile's limits.
`events_overflow` counts an over-budget event one frame after it fired (the next frame's event
prepare drops it), so read it a frame after the burst.

## Mesh shapes, kill volumes and ribbons (`.vfx.ron` v7)

- **Mesh shapes.** `MeshSurface(mesh: "...")` spawns on the surface of a mesh the game registered
  under that name (`VfxLibrary::add_cpu_mesh`), by area; `SkinnedMesh(mesh, max_per_frame)` spawns
  on the deformed surface for the instance's current joint palette (`VfxScene::set_skin_palette`),
  at most `max_per_frame` per frame. `Normal(speed)` launches along the surface normal. A mesh that
  is not registered, has no triangles or no area spawns nothing: `vfx.describe` shows
  `mesh_status` and `vfx.spawn_probe` shows `refused` and `mesh_error`. Check one: `vfx.spawn_probe`
  with `frames: 1`, then compare the positions with the mesh.
- **Kill volumes.** `kill_volume: Some((shape: Box(half_extents: (...)) | Sphere(radius: r),
  center: (...), kill: Inside | Outside))` kills particles that enter (or leave) the volume. It
  follows the instance's rotation and scale. The GPU and the CPU reference use one rule.
- **Ribbons.** `render: (mode: Ribbon, ribbon: (...))` draws the emitter's particles as one strip per
  instance (`link: Emitter`) or per frame's spawns (`SpawnGroup`), width and colour over length,
  `texture: Stretch` or `Tile(metres)`, at most `max_points`. A strip of `n` points is `6 (n - 1)`
  vertices. Ribbon particles live on the CPU; they are not in the GPU pool counters.

| Code | Where | Meaning |
|---|---|---|
| `missing_mesh` | `shape.mesh_status`, `mesh_error` | No mesh is registered under the shape's name. |
| `bad_mesh` | `shape.mesh_status`, `mesh_error` | The mesh is empty, has no area, a bad index, or is rigid under `SkinnedMesh`. |
| `missing_mesh` | `render.mesh_status` | No mesh is registered under the mesh particle's `render.mesh` (v8); it draws nothing. |
| `bad_mesh` | `render.mesh_status` | The GPU refused the mesh particle's mesh at the last upload (more than 65 536 triangles, or 64 render meshes already in use); it draws nothing. |

## Mesh particles from mesh assets (`.vfx.ron` v8)

`render: (mode: Mesh, mesh: Some("name"))` draws each particle as the mesh registered under
`name` (`VfxLibrary::add_mesh` or `add_cpu_mesh`, the same registry the mesh shapes use) instead
of the built-in cube, scaled by the particle size and drawn instanced from the mesh's own vertex
and index buffers. Without `mesh` the particle is the cube, as in v7. Check one:
`vfx.describe` shows `render.mesh_status` `ok` and the mesh's `indices` (the cube is 36); a
`missing_mesh` or `bad_mesh` status means that emitter draws nothing while its particles still
simulate (they count in `vfx.stats` `live_mesh`). Register the mesh before the first frame that
draws the effect; a mesh registered later draws from the next upload.

`lit: true` on the render module shades a mesh asset with the scene sun and ambient on its face
normals and draws its front faces only (counter-clockwise, glTF's winding); a mesh wound the other
way shows its inside, so flip its winding at import. `vfx.describe` shows `render.lit`. Each
registered mesh is one instanced draw over only that mesh's particles: `mesh_draws[].instances`
is how many the last frame drew (a mesh whose emitters are all dead draws 0). The pipeline warm
compiles the mesh-asset pipeline with the other effect pipelines, so the first mesh particle does
not compile on the frame path.

```json
{"method": "vfx.describe", "params": {"system": "shard_burst"}}
```

Examples are `samples/vfx_lab/effects/mesh_sparks.vfx.ron`, `skinned_aura.vfx.ron`,
`kill_room.vfx.ron` and `ribbon_trail.vfx.ron` (the lab registers `lab/plane` and `lab/arm`).

```json
{"method": "vfx.describe", "params": {"system": "sample_mesh_sparks"}}
{"method": "vfx.spawn_probe", "params": {"system": "sample_mesh_sparks", "seed": 7, "frames": 2, "max": 16}}
```

## Atmosphere-aware effects

Explosions and smoke follow the level's air by default (`LevelSettings::atmosphere.preset` and
`gravity`, the same source as `audio.medium`). `vfx.medium` reports it:
`{ atmosphere, gravity, wind, variant_suffix, variants }`, where `variants` lists the effects that
play a medium variant, for example `{ "effect": "explosion", "plays": "explosion@airless" }`.

- **airless** (Europa, the Moon): a flash, a fireball of the explosive's own gases that is gone
  by about 0.35 s, then ice and regolith ejecta on ballistic arcs under the level's gravity
  (no drag), a thin low dust skirt, and nothing that rises or billows. Landed ejecta rest where
  they fell and fade. `smoke` becomes a thin vent fountain that falls back.
- **mars**: the fireball dies fast; butterscotch dust billows, lingers for 10 s or more, rises
  weakly and drifts with the wind; the soot is thin and disperses fast.
- **earth**: the reference look (lit smoke columns, sooty fireballs).

A game's own `.vfx.ron` named `explosion@airless` (or `@mars`) replaces the built-in variant.
Spawn effects with a horizontal forward (the default +Z): the effects' local up is the
instance's up axis, which is world up only when forward is horizontal.

```json
{"jsonrpc":"2.0","id":7,"method":"vfx.medium"}
```

## Hover washes

A hovering hull kicks up the ground under it. Give the hull a `regia_vfx::HoverWash` (gameplay
sets `ground`, `ground_normal` and `height` every frame) and the plugin keeps one effect on it:

- **surface**: `dust` (Mars: butterscotch fines), `ice` (Europa: bright grains that glint),
  `frost` (fine powder), or `auto` (airless → ice, Mars and Earth → dust).
- **height**: intensity is `(1 − h / 2R)²` for footprint radius `R`: full on the ground, a
  quarter at one radius up, nothing past one diameter. Fewer and fainter particles, same look.
- **footprint**: the effect is authored for a 3 m radius and scales to the hull (sizes and offsets
  by `R / 3`, speeds by `√(R / 3)`).
- **air**: as for explosions. In air a rolling, lit skirt drifts and settles (Mars: lingers several
  seconds and drifts downwind). Airless: only grains, on ballistic arcs under the level's gravity,
  that land, with no lingering cloud.
- **look**: the outflow sheet and the billows are soft puffs that grow as they slow, drawn at most
  three times longer than wide along their motion, never as long streaks. A small grit spray and,
  on ice, glints ride on top.
- **jumps**: a cruising hull lends a share of its speed to the dust. A ground point that moves
  faster than 200 m/s (a respawn or teleport) moves the wash without that push, so the sheet is
  not flung away as streaks for a frame.

`vfx.hover_washes` lists them: `{ entity, surface, resolved_surface, footprint_radius, height,
intensity, expected_intensity, playing }`. `vfx.set_hover_wash` adds or changes one; fields you
leave out keep their value.

```json
{"method": "vfx.set_hover_wash", "params": {"entity": 4294967310, "footprint_radius": 2.4, "height": 0.6, "ground": [12.0, -3.1, 40.0]}}
```

Check it: `wait_until` `vfx.hover_washes` shows `playing`, then screenshot the hull. Raise `height`
to `2 × footprint_radius` and the wash stops (`intensity` 0). From above, the ring under the hull
should read as a soft skirt; thin radial lines (a "hairy ring") mean the wash is streaking.

## Thruster plumes

Give a nozzle entity (a child at the unit's socket) a `regia_vfx::ThrusterPlume`. The plume
follows the entity's transform, with the exhaust along `exhaust` (local, default down), at
`throttle`. Each air shows something different:

- **earth:** heat shimmer (refraction cards, Desktop High), a faint exhaust haze and the nozzle
  glow.
- **mars:** no visible shimmer (the air is about 1.5 % of Earth's density). A thin plume that
  spreads wider and drifts, and the glow.
- **airless:** the exhaust is invisible. Only the nozzle glows, and its light makes a lit spot on
  the ground when the unit is low.

Kinds are `duct` (gleaner lift/thrust ducts, Kraken fans) and `jetpack` (pilots). `vfx.plumes`
lists the plumes and DO27's thruster table (unit, kind, sockets, exhaust direction).
`vfx.set_plume` adds or changes one; throttle 0 is idle, with no particles and no light.

```json
{"method": "vfx.set_plume", "params": {"entity": 4294967330, "kind": "duct", "throttle": 0.8}}
```

The hover wash in air is a brownout-style outflow. A flat, stretched sheet streams out along the
ground (soft against it), then rolls up at the jet's edge into round billows that lift and drift.

## Weapon hits, muzzles and tracers

`vfx.hit` plays one round landing (`regia_vfx::WeaponHit`, also a command games queue):

- **metal**: hot sparks, a short flash, molten droplets, a burnt-paint wisp (in air), and a scorch
  decal with a molten spot that cools from yellow-white through orange to dull red in about a
  second.
- **concrete**: chips, a fan of grit, a lit dust puff, and a chip decal (a dark pit in lighter,
  freshly exposed aggregate).
- **glass**: glitter fragments and powder. With `target` set to the glass entity, its
  `GlassDamage` takes the impact in its own frame (`render.glass_damage` shows the crack level).

Decals ride `target` (a moving hull keeps its marks) and retire with it. Airless hits fly drag-free
and nothing puffs; sparks stay hot longer (no air to cool them). The calibre (`light`, `medium`,
`heavy`, or a DO27 `weapon` id) scales the effect and the decal, and sets the glass energy.

```json
{"method": "vfx.hit", "params": {"position": [3.1, 1.2, 0.4], "normal": [1, 0, 0], "surface": "glass", "weapon": "rifle", "target": 4294967321}}
```

`vfx.weapons` lists DO27's weapons and what each plays: muzzles `muzzle_flash` (rifles, pistols,
spike and ricochet guns), `muzzle_cannon` (chaincannon, sentry, guntower) and `muzzle_launcher`
(thumpgun, arcmortar, quakeshell, heatseeker); tracers `tracer` and `tracer_heavy`; hits by
calibre, explosive rounds as the atmosphere-aware `explosion`.

Muzzle flashes and tracers draw their color *over* the scene, so they read as orange on Europa's
bright ice in full sun instead of clipping to white. Their additive cores are small. Airless and on
Mars a muzzle has no secondary flash (no oxygen), and airless there is no smoke wisp. Tracers carry
their own oxidiser and look the same everywhere.

## Soft-particle layer (cost)

Dust, smoke and billows (soft, low-frequency alpha particles) draw into an offscreen layer at a
fraction of the frame size, and are composited back with a depth-aware bilateral upsample, so
they never bleed across a hull's silhouette. Sparks, tracers, muzzle cores and fire stay full
resolution. Tiers:

| Tier | Soft layer |
|---|---|
| Desktop High | half (the default) |
| Desktop Ultra | full |
| Mobile | quarter, always |

`vfx.get_look` reports `soft_particle_res` (the override, or `null`) and
`soft_particle_res_profile`. `vfx.set_look` takes `"soft_particle_res": "full" | "half" |
"quarter" | null`. The layer applies to HDR frames and is skipped when no soft particle is alive.

The alpha sort and the particle update cover only what can be alive: the live alpha count and
the ring slots spawned within the longest lifetime. Frames are unchanged.

## Heat haze and fire light

`vfx.get_look` reads the fire look: `{ heat_haze, haze_scale, fire_light, fire_light_scale,
fire_lights_live, strongest_light }`. `strongest_light` is `{ position, color, radius }` of
this frame's brightest particle light, or `null`.

`vfx.set_look` changes it; fields you leave out keep their value. It returns `{ before, after }`.

- `heat_haze` (bool): haze cards bend the scene behind the fire. They draw on any HDR frame
  where a scene copy exists (the host refreshes it after decals); elsewhere they are skipped.
- `haze_scale` (0..=4): multiplies every card's `heat_haze` pixels.
- `fire_light` (bool): particle lights reach the renderer's local lights (the ground under the
  fireball glows) and the lit smoke.
- `fire_light_scale` (0..=8): multiplies every particle light. One unit of
  `ParticleLightDef::intensity`, with envelope 1, instance intensity at least 1 and a brightest
  linear channel of 1, is 5e5 lm. A dimmer peak scales that down. The gathered colour is the
  linear colour times intensity, the fire envelope and `min(instance intensity, 1)`.

```json
{"method": "vfx.set_look", "params": {"heat_haze": false, "fire_light_scale": 1.5}}
```

Check it with a screenshot before and after: the haze shows as a wobble behind the fire, the
light as a warm patch on the ground. The look is runtime state, not saved in the level.

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
vfx.get_look {}                  # { heat_haze, haze_scale, fire_light, ... }
vfx.weapons {}                   # each weapon with its muzzle, tracer and hit effect
```

### More `vfx.set_hover_wash` parameters

The schema (`agent.schema`) also lists these optional parameters:

- `enabled` (boolean): false: no new particles.

### More `vfx.hit` parameters

The schema (`agent.schema`) also lists these optional parameters:

- `seed` (integer): Variation seed.

### More `vfx.set_plume` parameters

The schema (`agent.schema`) also lists these optional parameters:

- `enabled` (boolean): false: no particles and no light.

## Particles under temporal upscalers

Effects draw after the temporal resolve (TAAU, TSR, FSR 3.1, a vendor upscaler), into a copy of its
output, so they never enter an upscaler's history: a spark or a smoke puff over a moving background leaves
no trail, at any render scale, and needs no reactive mask. `render.get_upscalers` reports this as
`reactive_mask.layers.placement: "after_resolve"`. The check is `cargo test -p regia_vfx --test
reactive_upscaler`; the design and the frame-generation follow-up are in ADR-3101 and the
[upscalers page](upscalers.md#particles-and-the-reactive-mask).

## Particle budgets and order

`vfx.stats` reads the particle budgets this frame (read-only):

```json
{"jsonrpc":"2.0","id":9,"method":"vfx.stats"}
```

It returns `{ capacity, smoke_budget, sorted, live, live_alpha, live_mesh, headroom,
smoke_headroom, last_scheduled, pending_jobs, instances }`:

- `capacity` is the pool size (the global budget) and `smoke_budget` the cap on live
  alpha-blended particles, both from the quality profile (Desktop High 100 000 and 8 192,
  Mobile 10 000 and 2 048). `headroom` and `smoke_headroom` are what is left of them.
- `live` counts every particle the scheduler has spawned and not yet retired, including the
  `pending_jobs` that wait for the next drawn frame. A spawn counts once. A particle that
  dies this frame frees its budget this frame.
- `instances` lists each live effect: `{ id, effect, live, budget, active, age }`, where
  `budget` is the system's `system.budget`. `live` never passes `budget`.
- `sorted` is `true` when the profile sorts. Alpha-blended and mesh particles then draw back
  to front. Two particles at the same depth keep a fixed order (by pool slot), so a column of
  smoke does not shimmer, and the same scene draws in the same order on every run. When it is
  `false` (the Mobile profile) the alpha-blended particles draw emitter by emitter, back to front
  for the current view: the CPU ranks the emitters by the depth of their effect's origin each
  frame, so of two overlapping smoke effects the nearer one is on top; inside one emitter the
  particles keep their gather order.

Particle simulation is deterministic: the same seed, the same timestep and the same spawns put
every particle in the same pool slot with the same position, on every run and with either
alpha sort (bitonic, the default, or the opt-in radix sort). Additive particles are not sorted;
they add in whatever order the GPU gathers them, which can change the last bit of a pixel.

Check it: spawn an effect, then `wait_until` `vfx.stats` shows `live` above 0 and each
instance's `live` at or below its `budget`. Stop it and `live` falls to 0 once its longest
lifetime has passed.

Errors: without `regia_vfx::VfxPlugin` the method answers `UNSUPPORTED` with data code `F0016`.

Sample effects for these checks live in `samples/vfx_lab/effects/`: `sample_smoke_stack` (lit
puffs at equal depths), `sample_debris_cubes` (overlapping mesh debris), `sample_budget_storm`
(sparks asked for at more than twice their budget) and `sample_ember_drift` (curl, vortex and attractor embers).
`cargo run -p vfx_lab -- --effects samples/vfx_lab/effects` runs the lab's format, determinism
and budget checks on them.

## VFX editor (`editor.vfx_*`)

The editor's **VFX Editor** dock tab edits a `.vfx.ron` effect: the emitter list,
each emitter's module stack with a palette, enable boxes (the A/B toggle) and parameter rows, the
curve and gradient editors, a live preview and a per-emitter cost table (spec 26 § 3, ADR-3171). The
four methods below drive the same code. Every change is one step on the editor's undo stack (shared
with every other panel; `editor.undo` works too) and is checked by the `.vfx.ron` validator: an edit it
refuses changes nothing, not the effect, the undo stack or the file.

**Modules.** `.vfx.ron` v7 has no module list; an emitter is a fixed spawn, init, update and render
block. The editor shows a module wherever a group of fields leaves its default: `spawn`, `init` and
`render` (always there, `required: true`), `init_color`, `gravity`, `drag`, `curl`, `vortex`,
`attractor`, `wind`, `size_over_life`, `color_over_life`, `alpha_over_life`, `rotation_over_life`,
`kill_below`, `kill_volume`, `depth_collision`, `events`, `flipbook`, `soft_particles`, `light` and
`heat_haze`, in that order. `palette` lists what `add_module` can add (everything optional except
`events`, which needs a child emitter: write the link in the file). Module order inside an emitter is
fixed by the format; `move_emitter` reorders emitters.

**Disable (A/B).** `disable_module` takes the module's values out of the effect into the editor (the
format has no "enabled" field and stays frozen); the preview, `sim` and a save see the effect without it.
`enable_module` puts the exact values back, so the `sim` hash returns to what it was. A disabled module
is listed with `enabled: false`, cannot be edited until it is enabled, and is not written by `save`
(`disabled_not_saved` counts them).

**Curves.** Curves interpolate linearly between keys and have no tangents. Keys stay sorted by `t`
(0..=1); a moved key that passes a neighbour is re-sorted. Scalar curves: `size_over_life`,
`alpha_over_life`, `rotation_over_life`, `ribbon_width`. Gradients (`#rrggbb` or `#rrggbbaa`):
`init_color`, `color_over_life`, `ribbon_color`. Adding a key to an empty curve adds its module.

**Saving.** `save` writes atomically. An effect nobody edited is not rewritten; an effect undone back
to the state it was opened in saves the file's original bytes; anything else is written in the
canonical form (`VfxSystemDef::to_ron`, the form the loader round-trips).

`editor.vfx_open { path }` opens a project-relative `.vfx.ron` and returns the state:
`{ open, path, dirty, revision, name, version, budget, emitters: [{ index, name, draw_list, modules: [{
kind, stage, enabled, required, params: [{ name, kind, value, min?, max?, choices? }] }], curves: {
size_over_life: [[t, v]], ... }, gradients: { init_color: [[t, "#rrggbb"]], ... } }], disabled, palette,
undo_depth, redo_depth }`. A parameter's `kind` is `number`, `integer`, `boolean`, `choice` or `color`;
numbers outside `min..=max` are refused. `keys` and `links` rows are read-only counts.

`editor.vfx_state { sim_steps?, cost_time?, seed? }` returns the same state. With `sim_steps` it also
runs the CPU reference simulation (`regia_vfx::probe::probe_steps`, events and kills included) for that
many 1/60 s steps from a fixed seed (default 1) and returns `sim: { steps, seed, particles, hash }`:
the hash covers every live particle's emitter, position, velocity, age and lifetime bits, so two effects
with the same hash simulate identically. With `cost_time` it returns the cost rows (below) at that time.

`editor.vfx_edit { op, ... }`, where `emitter` is a name or an index:

| `op` | Parameters |
|---|---|
| `add_module`, `remove_module`, `enable_module`, `disable_module` | `emitter`, `module` |
| `set_param` | `emitter`, `module`, `param`, `value` (number, boolean or string) |
| `curve_add_key` | `emitter`, `curve`, `t`, `v` |
| `curve_move_key` | `emitter`, `curve`, `index`, `t`, `v` |
| `curve_delete_key` | `emitter`, `curve`, `index` |
| `gradient_add_key` | `emitter`, `gradient`, `t`, `color` |
| `gradient_move_key` | `emitter`, `gradient`, `index`, `t`, `color` |
| `gradient_delete_key` | `emitter`, `gradient`, `index` |
| `add_emitter` | `name` (a 20/s point fountain to start from) |
| `remove_emitter` | `emitter` |
| `move_emitter` | `emitter`, `to` |
| `set_budget` | `value` |
| `set_fixed_bounds` | `min`, `max`: `[x, y, z]` each, metres in the effect's local space (`min <= max`) |
| `clear_fixed_bounds` | none (the computed bounds cull again) |
| `fixed_bounds_from_measured` | `time?` (default the preview time), `seed?` (default the preview seed) |
| `save`, `undo`, `redo` | none |

It returns the state plus `changed` (false when the edit was a no-op), `written` and
`disabled_not_saved` for `save`, `undone` / `redone` for `undo` / `redo`.

`editor.vfx_preview { time?, seed?, width?, height?, png?, bounds? }` renders the open effect through the
offscreen VFX harness (`regia_vfx::offscreen`: a sky, a lit ground and the full GPU simulation and
draw) at a fixed 1/60 s step, fired 0.5 m above the ground with local +Z up and framed by the CPU
probe's envelope. `time` is 0..=30 s (default 1), the size 32..=1024 per side (default 256×144). It
returns `{ path, revision, time, steps, seed, width, height, coverage, gpu_alive, counters: { additive,
alpha, mesh, alive }, gpu_ms, costs, png? }`. `coverage` is the fraction of pixels the effect changes
against the same backdrop without it. A later time steps the running preview on; an earlier time, a
new seed, a new size or any edit restarts it, so the same effect, seed and time always take the same
number of steps. With `png: true` the frame is written to a temp directory
(`<tmp>/regia-vfx-preview/`) and `png` is its path. With `bounds: true` it also returns `gpu_bounds`
(see "Fixed, measured and GPU bounds" below).

**Cost rows** (`costs`), one per emitter: `{ emitter, alive, spawned_per_sec, draw_list, draw_calls,
gpu_ms, gpu_source }`. `alive` comes from the CPU reference simulation at that time;
`spawned_per_sec` counts the scheduler's spawns in the one-second window that ends at that time (a
burst at t = 0 counts up to `cost_time` 1; `null` for an event-driven emitter). `draw_list` is the list the emitter feeds (`additive`, `alpha`, `mesh`, `ribbon` or `haze`);
each list is one indirect draw shared by its emitters, so `draw_calls` is 1 when any emitter of the list
has a live particle. `gpu_ms` is the emitter's share (by live particles) of the timed VFX simulate and
draw passes, when the device has timestamp queries and a preview frame was timed; otherwise it is
`null` and `gpu_source` is `cpu_counts`.

Check it: `editor.vfx_open`, then `editor.vfx_state { sim_steps: 90 }` and keep `sim.hash`;
`editor.vfx_edit { op: "disable_module", emitter: 0, module: "gravity" }` changes the hash and
`enable_module` brings it back; `editor.vfx_preview { png: true }` shows `coverage` above 0.

Errors (`data.reason`, with `data.hint` pointing here):

| Code | When |
|---|---|
| `vfx_editor_no_project` | No project is open. |
| `vfx_editor_bad_path` | The path leaves the project or does not end in `.vfx.ron`. |
| `vfx_editor_io_failed` | The file (or the preview PNG) cannot be read or written. |
| `vfx_editor_parse_failed` | The loader refuses the file (the message is the loader's). |
| `vfx_editor_no_effect` | No effect is open. |
| `vfx_editor_unknown_emitter` | No emitter by that name or index. |
| `vfx_editor_unknown_module` | Not a module kind. |
| `vfx_editor_unknown_param` | The module has no such parameter in its current mode, or it is read-only. |
| `vfx_editor_unknown_curve` | Not a curve or gradient name. |
| `vfx_editor_bad_value` | A value outside its range, non-finite, of the wrong type, an unknown choice, a bad colour, a key `t` outside 0..=1, or a preview size or time out of range. |
| `vfx_editor_bad_index` | No key, or no emitter position, at that index. |
| `vfx_editor_too_many` | Past 64 keys on a curve or 64 emitters. |
| `vfx_editor_required_module` | `spawn`, `init` and `render` cannot be removed or disabled. |
| `vfx_editor_not_addable` | `events` cannot be added from the palette. |
| `vfx_editor_already_present` | The emitter has that module (enabled or disabled). |
| `vfx_editor_not_present` | The emitter does not have that module. |
| `vfx_editor_module_disabled` | The module is disabled: enable it before editing it. |
| `vfx_editor_invalid` | The `.vfx.ron` validator refuses the result (its message says why: `min > max`, a duplicate emitter name, a broken event link, ...). |
| `vfx_editor_serialize_failed` | The effect cannot be serialized. |
| `vfx_editor_no_gpu` | No GPU device for the preview. |
| `vfx_editor_preview_failed` | The harness or a preview frame failed on the GPU, or its GPU bounds could not be read back. |
| `vfx_editor_empty_bounds` | `fixed_bounds_from_measured`: nothing is alive at that time and seed to measure. |

`vfx.spawn_probe` requires only `read` (ADR-4760). It borrows the world immutably and uses
a scratch instance: live instances, loaded systems and entities remain unchanged. Repeated
calls with the same seed return the same positions. `vfx.spawn` still edits the live scene
and requires its existing permission.

## Scene budget and significance

Every system has its own `budget` and distance `lod`. The scene-wide budget ranks the live
instances of the whole scene, so a battle with 300 effects on a phone pays for the ones that
matter (ADR-3275). It is off until caps are set: without caps the scene schedules exactly as
before.

Each update scores every active instance: `priority × projected size × visibility × youth`.
Projected size is the bound of the system's particles over the distance (nearer and bigger
scores higher). Out of view scores a quarter. An effect that just started scores up to 1.5×,
fading over a second. Priority is 1 unless overridden per system or per instance. Instances
then take a state by rank:

| State | What it does |
|---|---|
| `full` | The first `full_instances`: spawns at its authored rate. |
| `reduced` | The rest up to `max_instances`: rate emitters at `reduced_rate` (bursts keep their counts). |
| `paused` | Out of view with `pause_offscreen`: spawns nothing; a burst due now is spent unseen. On re-entry it catches up at most `warmup_s` of its rate, never more than `warmup_max_particles`. Holds no rank slot. |
| `culled` | Past `max_instances`: spawns nothing; its live particles finish. |

Stopped or spent instances are `finishing` and not ranked. An incumbent ranks with a
`hysteresis` bonus (default 25 %) and keeps its state for `min_dwell_frames` (default 30), so
an effect at the cut does not flicker; instances coming back into view and demotions a lowered
cap needs are never held, and a new instance that outranks a held one (bonus included) takes its
slot on its first ranking while one that does not waits (ADR-7431). `max_spawns_per_frame` is shared in rank order,
`max_live` lowers the pool capacity. A per-system cap limits active instances of one system;
its `steal` policy decides what a spawn past it does: `reject` (refused), `oldest` (the oldest
is stopped), `least_significant` (the least significant is stopped, or the newcomer refused if it
scores lower).

Tier presets (`regia_vfx::quality::significance_caps`, design values):

| Tier | Instances | Full | Spawns / update | Live |
|---|---|---|---|---|
| MobileLow | 16 | 6 | 256 | 4 000 |
| MobileHigh | 32 | 12 | 512 | 10 000 |
| Deck | 48 | 16 | 1 024 | 20 000 |
| Low | 32 | 12 | 512 | 20 000 |
| Medium | 96 | 32 | 2 048 | 60 000 |
| High | 160 | 64 | 4 096 | 100 000 |
| Ultra | 256 | 128 | 8 192 | 100 000 |

All tiers pause off screen with a 0.25 s warm-up. A host applies them with
`scene.significance_mut().set_caps(regia_vfx::quality::significance_caps(tier))`.

- `vfx.significance` (`limit`, 1..=1000, default 64) returns `{ enabled, caps, counts: { full,
  reduced, paused, culled, finishing }, rejected_spawns, rejected_total, stolen_total,
  transitions, transitions_total, spawned, warmup_particles, spawn_cap_reached, system_priority,
  instance_priority, instances, ranked }`. `ranked` lists live instances most significant first:
  `{ rank, id, system, state, finishing, score, distance, visible, priority, age, live,
  frames_in_state }`. Without caps `state` is null and the scores are computed now.
- `vfx.significance_set` applies, in this order: `tier` (a quality tier name, or `off` for no
  caps), `caps` (a patch: a null cap is cleared), `system_cap` (`{ system, max_instances,
  steal }`; a null `max_instances` removes it) and `priority` (`{ system | instance, value }`; a
  null value removes it). Returns `{ before, caps }`.

```json
{"method": "vfx.significance_set", "params": {"tier": "Low", "priority": {"system": "objective_flare", "value": 50}}}
{"method": "vfx.significance", "params": {"limit": 10}}
```

Check it: set a tier, spawn many effects, `wait_until` `vfx.significance` shows `counts.full +
counts.reduced` at or below `caps.max_instances`, and `ranked` with the nearest (or overridden)
instances on top. Turn the camera away: their state goes `paused` and `vfx.stats` shows no new
spawns from them.

| `data.reason` | Meaning |
|---|---|
| `unknown_tier` | `tier` is not a quality tier name or `off`. |
| `unknown_cap` | `caps` holds a field that is not a cap. |
| `unknown_steal_policy` | `system_cap.steal` is not `reject`, `oldest` or `least_significant`. |
| `refused_by_cap` | `vfx.spawn` of a loaded system that its per-system cap refused. |
| `value_out_of_range` | A number out of its range, or a field of the wrong type. |
| `missing_param` | Nothing to apply, or a required field of `system_cap` / `priority` is absent. |
| `bad_source` | `priority` names both or neither of `system` and `instance`. |

All are `F0002`. A priority for a system that is not loaded or an instance that is not live is
`F0003`; a world without `regia_vfx::VfxPlugin` is `F0016`.

## System overview, emitter timeline and bounds (W12)

The System overview displays one card per emitter with a spawn summary and measured alive/draw
costs. **Enable preview** and **Solo preview** isolate emitters without editing the document,
changing the undo stack or changing saved content. Multiple soloed emitters play together;
a disabled emitter stays disabled even when soloed. Reopening an effect clears isolation.

`editor.vfx_edit { "op": "preview_emitter", "emitter": "sparks", "enabled": true, "solo": true }`
uses the same operation. Both booleans are optional (omission keeps the current value).
`editor.vfx_edit { "op": "scrub", "time": 0.75 }` pauses and seeks the panel preview (0..=30 s).
These operations still require an open effect; invalid time is `vfx_editor_bad_value`.

`editor.vfx_state` adds `overview`, `preview_time` and `timeline`. Timeline rows contain
`name`, `start`, `spawn_end`, `alive_until`, `lifetime: [min, max]`, `burst_count`, `count_scale`
and `event_driven`. A burst ticks at its delay; a bar includes the maximum particle lifetime.
Unbounded rate emitters have null end times; event emitters have no invented start or duration.
The panel plots the first 30 seconds and provides a named scrub slider.

Pass `bounds_time` and optional `seed` to `editor.vfx_state` for `bounds` (see "Fixed, measured
and GPU bounds" below for what the boxes hold): `mode: "computed"` (or `"fixed"` when the effect has
fixed bounds), `fixed: { min, max } | null` (local space), `fixed_world: { min, max } | null` (the
fixed box at the preview instance), `kind: "particle_extents"`, `source: "cpu_reference"`,
`space: "preview_world"`, quantized `time`, `seed`, `system: { min, max } | null`, and
`emitters: [{ name, aabb: { min, max } | null }]`.
Empty emitters have null bounds; the system AABB is the union of the nonempty emitters.
The preview draws the emitter and system boxes with theme tokens at the measured fixed step.

Keyboard: Tab reaches card selection, Enable preview and Solo preview; Space toggles a focused
checkbox. Tab reaches Scrub preview; arrow keys move time. Controls follow `appearance.ui_scale`.
No new content format or golden is introduced. Tests: `w12_state_reports_emitter_timeline_and_computed_bounds`,
`w12_overview_enable_solo_and_scrub_are_view_only`, and
`w12_overview_and_timeline_operate_by_keyboard_at_two_scales`.

In the panel, expand **System overview** or **Emitter timeline** to show these controls.
The disclosure headers accept Tab and Space; collapsed sections keep the existing module
and curve editors visible in a small dock.
Preview isolation also applies to `vfx_state` simulation and cost diagnostics and to `vfx_preview`.

## Fixed, measured and GPU bounds (P-225)

**Measured bounds** are the CPU reference run with every motion module of the GPU update kernel
(gravity, drag, scene wind, curl noise, vortex, attractor, the kill plane and volume), each live
particle grown by its drawn extent: a camera-facing quad of width `s` reaches `s·√2/2` at any
rotation, a stretched quad half its diagonal (`max(velocity_scale·speed, min_length)` by `s`), a mesh
particle (the built-in cube of edge `s`) `s·√3/2`. Not included: the screen-size floor (a per-view
enlargement) and a registered render mesh larger than the unit cube. Depth collision uses the flat
ground stand-in of the reference.

**Fixed bounds** are `system.bounds` of the `.vfx.ron` (a v8 fold, `docs/formats/vfx-ron.md`): a box
in the effect's local space. When set, the box transformed by the instance culls the effect (the
scene significance ranking decides in view, distance and projected size by it) and the readout
reports `mode: "fixed"`. It never kills particles. `editor.vfx_edit` sets it (`set_fixed_bounds`),
clears it (`clear_fixed_bounds`) or sets it to the box holding the measured system bounds at a time
(`fixed_bounds_from_measured`); each is one undo step, validated like a hand edit (a non-finite
value or `min > max` is `vfx_editor_bad_value`). `editor.vfx_open` and `editor.vfx_state` report it
as `fixed_bounds: { min, max } | null`.

**GPU bounds** reduce the live GPU pool to one box per emitter: the same extent rule, `atomicMin` and
`atomicMax` on an order-preserving float key, read back once (`regia_vfx::VfxGpu::read_particle_bounds`).
The read blocks on the device, so it is opt-in: `editor.vfx_preview { bounds: true }` returns
`gpu_bounds: { source: "gpu_reduction", kind: "particle_extents", space: "preview_world",
system, emitters: [{ name, aabb }] }`, and the panel's **Read GPU bounds** checkbox reads it at most
once per half second of preview time. A game never runs it.

In the panel, under the bounds readout: **Read GPU bounds**, **Fixed bounds** (turns the box on, sized
from the measured bounds, or off), the six **Min** / **Max** fields and **Set fixed bounds from
measured**. The preview draws the fixed box with the gizmo ring token and a handle on each face in
the axis tokens; dragging a handle moves that face along its local axis (one undo step per drag).

Keyboard: Tab reaches each checkbox, field and the button; Space toggles a checkbox and presses the
button; ArrowUp and ArrowDown step a focused field. The handles are the mouse route of the fields.
Controls follow `appearance.ui_scale`.

Check it: `editor.vfx_open`, `editor.vfx_edit { op: "fixed_bounds_from_measured", time: 1.0 }`, then
`editor.vfx_state { bounds_time: 1.0 }` shows `mode: "fixed"` and a `fixed_world` box holding
`system`; `editor.vfx_preview { time: 1.0, bounds: true }` returns `gpu_bounds`.
Tests: `measured_bounds_include_particle_extent`, `an_effect_with_fixed_bounds_culls_by_them`,
`gpu_bounds_contain_every_particle`, `w13_fixed_bounds_set_from_measured_saved_and_reported`,
`w13_fixed_bounds_operate_by_keyboard_at_two_scales`, `w13_fixed_box_gizmo_drags_one_face_with_theme_tokens`.
