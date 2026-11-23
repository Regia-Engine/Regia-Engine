# Cloth and ropes (agents)

`physics.cloth_stats` reports the CPU cloth and rope solvers of the running app
(`regia_physics::cloth`, see [Cloth and ropes](../user-guide/cloth.md)). It is read-only and needs
`regia_physics::cloth::ClothPlugin`. `AgentPlugin` registers the method on the `AgentExtension` in every
app with the agent (ADR-1809; `regia_agent::cloth::install_cloth_methods` does the same for a host that
builds its own extension). Without the plugin it answers `{ "plugin": false, "solvers": [] }`.

| Method | Params | Result |
|---|---|---|
| `physics.cloth_stats` | `name?` | `{ plugin, solvers: [{ name, particles, pinned, distance_constraints, bend_constraints, colliders, steps, dropped_steps, contacts, max_stretch, bounds_min, bounds_max, digest }] }` |

- `plugin` is `false` and `solvers` is empty when the app has no `ClothSet` resource.
- `name` limits the answer to one solver. An unknown name answers `F0003` (not found); a `name` that
  is not a string answers `F0002` (invalid params).
- `solvers` is in insertion order, which is also the stepping order.
- `pinned` counts particles with inverse mass 0. `distance_constraints` includes stretch, shear and the
  skip-one bending links of ropes; `bend_constraints` counts dihedral hinges (cloth only).
- `steps` counts fixed steps run since the solver was made; `dropped_steps` counts steps skipped
  because a frame needed more than `max_steps_per_call`. A growing `dropped_steps` means the frame
  rate is too low for the cloth's step rate.
- `contacts` is the number of particles touching a collider in the last substep.
- `max_stretch` is the largest current-over-rest length ratio over the distance links (1 = rest).
  Stiff cloth (compliance 0) stays within about 2 % of 1 when settled.
- `bounds_min` / `bounds_max` are the particles' world-space box as `[x, y, z]`, or `null` for a
  solver without particles.
- `digest` is a 16-digit hex FNV-1a digest of every particle's position, velocity and inverse mass
  plus the step count. Two runs of the same build with the same inputs give the same digest at the
  same step.

## Checks agents run

- **It settled:** call `physics.cloth_stats` twice a second apart (`play.step` with a fixed step
  count between the calls); `bounds_min`/`bounds_max` should not move by more than a centimeter.
- **It touched the collider:** `contacts > 0` once the cloth reaches a sphere, capsule or plane.
- **It is deterministic:** run the same scripted session twice and compare `digest` at the same
  `steps`.
- **It is not over-stretched:** `max_stretch` near 1 for stiff cloth; values far above 1 mean too
  few substeps for the load.

## Errors the solver API answers

The method itself only answers `F0002` and `F0003`. Game code that builds cloth gets a typed
`ClothError`, whose `code()` is one of these protocol-safe strings (scripts that forward cloth
errors use them):

| Code | Meaning |
|---|---|
| `cloth_non_finite` | A position, velocity, mass, compliance, collider value or `dt` was NaN or infinite. |
| `cloth_negative` | An inverse mass, compliance, radius, friction, damping or `dt` was negative. |
| `cloth_particle_out_of_range` | A particle index does not exist. |
| `cloth_collider_out_of_range` | A collider index does not exist. |
| `cloth_repeated_particle` | A constraint names one particle twice. |
| `cloth_degenerate` | A zero plane normal, a grid under 2 × 2, parallel grid axes, zero rope segments or a zero-length rope. |
| `cloth_invalid_config` | A zero or too-large substep or iteration count, a non-positive time step, or zero steps per call. |

A rejected call changes nothing.

## Spec 55 methods

The same `install_cloth_methods` call registers seven more methods (`docs/specs/55-cloth-and-soft-body.md` § 5).
`target` is `{ name }` of a cloth solver (`ClothSet`) or a soft body (`SoftSet`, see
[soft bodies](soft-bodies.md)), or `{ entity }` of an entity with a `ClothSim` or `SoftSim` component (the solver
the framework glue built for it; an entity without one answers `F0003` with reason `cloth_not_found`).

| Method | Class | Params | Result |
|---|---|---|---|
| `physics.cloth_inspect` | read | `target?`, `particles?: { from, count <= 256 }` | `{ plugin, parallel, solvers: [{ name, kind, ...cloth_stats fields, state, blend, lod, dropped_self_pairs, self_contacts, self_crossings, max_layer, layered, layer_gap, torn, wind, max_pose_error, teleports, tethers, model?, volume?, rest_volume?, pressure?, inverted_tets?, particles?, instance? }], failed: [{ entity, asset, error }] }` |
| `physics.cloth_set_params` | play | `target`, `patch` | `{ applied: [key], params }` |
| `physics.cloth_pin` | play | `target`, `particles: [u32]` (at most 1,024) | `{ pinned }` |
| `physics.cloth_unpin` | play | `target`, `particles`, `inv_mass?` | `{ unpinned, pinned }` |
| `physics.cloth_wind` | play | `target?`, `velocity: [x, y, z]`, `gust?`, `clear?` | `{ wind }` |
| `physics.cloth_step` | play | `target?`, `count: 1..=600` | `{ steps_run, digest }` |
| `physics.cloth_reset` | play | `target` | `{ state }` |

- `physics.cloth_inspect` without a plugin answers `{ plugin: false, solvers: [] }`, like `physics.cloth_stats`.
  `kind` is `cloth` or `soft`. `state` is `inactive`, `settling`, `running`, `fading` or `paused`. `blend` is the
  blend the output uses (the game's blend times the settle/fade ramp). `lod` is 0, 1, 2 or 3 (off). `wind` is the
  wind velocity of the last step. `max_pose_error` is the largest distance from a free pose-bound particle to its
  target (0 without a pose). `torn` counts links cut by tearing. Soft bodies add `model` (`pressure`, `tetra`,
  `shape_match`), `volume`, `rest_volume`, `pressure` and `inverted_tets`. `particles` lists `{ index, position,
  velocity, inv_mass }`.
- `physics.cloth_set_params` `patch` keys: `damping`, `substeps`, `iterations`, `collision_thickness`,
  `gravity_scale`, `pull_compliance`, `pull_scale`, `inertia_scale`, `strain_cap` (`{ max, min? }` or `null`),
  `tether` (scale or `null`), `self_collision` (`{ mode: "off" | "spheres" | "vertex_triangle", radius }`),
  `self_friction`, `exclude_rings`, `wind_model` (`{ mode: "push", gain }` or `{ mode: "aerodynamic", drag, lift }`),
  `air_density`, `teleport_distance`, `tear` (`{ strain, max_per_step }` or `null`), `blend`, `lod`, `pressure`
  (soft bodies with the pressure model), and since ADR-9070: `gravity_override` (`[x, y, z]` or `null`; replaces the
  config gravity, still scaled by `gravity_scale`), `stretch_compliance` and `bend_compliance` (at least 0; every
  distance link, shear links included, or every bend link of the running solver; `params` echoes the first link's
  value), `lod_distances` (three ascending positive meters `[d1, d2, d_off]`, written on the instance's `ClothSim` or
  `SoftSim`; a solver built by code has no instance and refuses it) and `plasticity` (shape-matching soft bodies, see
  [Soft bodies](soft-bodies.md#plasticity)). `shear_compliance` (cook time only) and `friction` (reserved) are refused
  with `cloth_bad_param` (ADR-2250). The patch is applied as a whole or not at all; `params` echoes every
  setting after the call. On a soft body, `params.pressure` is the stored pressure (`null` when the model
  has none), including when this patch did not set it. `substeps`, `iterations`, `exclude_rings`, and
  `tear.max_per_step` must fit in an unsigned 32-bit integer (at most 4,294,967,295), in addition to their
  solver-specific limits. Larger integers return `F0002` with reason `cloth_bad_param` before any setting changes.
- `physics.cloth_unpin` gives the particles `inv_mass` (positive), or the mean inverse mass of the solver's free
  particles when absent (1 when there are none).
- `physics.cloth_wind` with a `target` sets that solver's own wind (and `gust`: `{ amplitude 0..1, period_steps >= 1,
  seed }`); without one it sets the global wind every cloth solver feels. `clear: true` zeroes the wind and drops the
  gust. `gust.period_steps` and `gust.seed` must fit in an unsigned 32-bit integer; larger values return
  `F0002` with reason `cloth_bad_param` before wind or gust changes. Wind is clamped to 60 m/s when used.
- `physics.cloth_step` runs fixed steps now, also while the solver is paused, for that solver only (or every solver
  without a target). `digest` is that solver's 16-digit hex digest; without a target it is the FNV-1a digest of
  every solver's digest in order (cloth solvers, then soft bodies).
- `physics.cloth_reset` snaps the solver to its pose (or, without a pose, to the positions its particles were added
  at), zeroes every velocity and answers `{ state: "settling" }`.

### Reasons

Errors of these methods carry `data.reason`:

| Code | Reason | When |
|---|---|---|
| `F0003` | `cloth_not_found` | No solver or soft body with that name, an `{ entity }` without a built `ClothSim`/`SoftSim`, or (`physics.cloth_edit_map`) no `ClothSim` using that asset. |
| `F0003` | `cloth_plugin_missing` | A play method in an app without `ClothPlugin`. |
| `F0002` | `cloth_bad_param` | A missing or malformed param, an unknown patch key, a value out of range. |
| `F0002` | `cloth_particle_out_of_range` | A particle index past the solver's particles. |
| `F0002` | `cloth_step_count_too_large` | `count` over 600. |
| `F0002` | `cloth_non_finite`, `cloth_bad_blend`, ... | The solver refused a value; the reason is its `ClothError` code. |

### Spec 55 checks

- **It settles:** `physics.cloth_step 300`, `physics.cloth_inspect`, `physics.cloth_step 60`, inspect again: bounds
  and `max_pose_error` move by less than 1 cm.
- **It is deterministic:** `physics.cloth_reset`, `physics.cloth_step N`, record `digest`, repeat, compare.
- **Wind works:** `physics.cloth_wind`, `physics.cloth_step 120`; `bounds_min`/`bounds_max` shift downwind.
- **It woke up:** `play.wait_until` on `state == "running"` after a reset (8 settle steps).

### More solver error codes

Game code that drives the spec 55 API gets these `ClothError` codes too:

| Code | Meaning |
|---|---|
| `cloth_pose_size_mismatch` | A pose target list does not have one entry per particle. |
| `cloth_bad_blend` | A blend outside `[0, 1]`. |
| `cloth_bad_param` | A tuning, map or soft-body value outside its range. |
| `cloth_map_size_mismatch` | A pose map is neither empty nor one value per particle. |
| `soft_not_closed` | A pressure soft body's surface has an edge not shared by exactly two triangles. |
| `soft_inverted_rest` | A tetrahedron is flat or inverted at rest. |
| `soft_bad_cluster` | A shape-matching cluster has fewer than 4 particles, repeats, or is flat. |
| `cloth_snapshot_too_large` | A world snapshot has more than 4,096 triangles or a hull with more than 64 planes. |
| `cloth_asset_invalid` | A `.cloth.ron`, `.soft.ron` or cooked payload is malformed (parse error, unknown map name, bad mesh, index out of range, truncated). |
| `cloth_unsupported_version` | A cloth or soft file is newer than this build reads. |

## Cloth assets and the game world

Cooked cloth (`.cloth.bin`, from a `.cloth.ron`) and soft bodies (`.soft.bin`, from a `.soft.ron`) reach the world
through the `regia_framework::cloth` components (`docs/formats/cloth-ron.md`, ADR-2083):

- `ClothSim { asset, skinned_mesh, layer, blend, lod_distances, reaction, section, .. }` on an entity: the glue
  builds a solver named `cloth:<entity bits>` (or `ClothSim.solver`) in `ClothSet`, skins the sim mesh with the
  owner's palette every fixed step, feeds body proxies (`ClothBodyProxy`) and a world snapshot of nearby rigid
  colliders, pauses it when the sim entity's `InheritedVisibility` is present and false (a present visible
  value does not consult the section; otherwise the section's `InheritedVisibility` is used; `ViewVisibility`
  is not written, ADR-2083), picks its distance LOD from the camera, and
  publishes the visible section to the `section` entity's `MeshRenderer`. `SoftSim` does the same for soft bodies
  (solver `soft:<entity bits>` in `SoftSet`).
- Every `physics.cloth_*` method takes `{ entity }` of such an entity as its `target`.
- `physics.cloth_inspect` adds `instance` to a solver that has one: `{ entity, asset, section, section_vertices,
  section_triangles, hidden_triangles, error }` (`section_*` count the mesh the section draws now, `hidden_triangles`
  the render triangles tearing dropped, `error` the code of the last pose failure). Its top-level `failed` lists
  instances with no solver as `{ entity, asset, error }`: a `ClothSimSource` or `SoftSimSource` that did not load, or
  an asset that did not build.

### `physics.cloth_edit_map`

| Method | Class | Params | Result |
|---|---|---|---|
| `physics.cloth_edit_map` | edit_scene | `asset` (the asset's name), `map`, `edit` | `{ changed, instances, dirty }` |

Edits one vertex map of LOD 0 of the cloth asset named `asset` (coarser LODs are resampled by nearest rest vertex)
and gives the edited asset to every `ClothSim` that uses it; their solvers rebuild and settle. `map` is `reach`,
`standoff`, `pose_pull`, `sim_mix`, `mass_scale` or `layer`. `edit` is one object with an `op`:

| `op` | Fields | Effect |
|---|---|---|
| `set` | `vertices: [[index, value], ...]` | Explicit values. |
| `sphere` | `center: [x, y, z]`, `radius`, `value`, `falloff: "linear" \| "smooth" \| "hard"`, `strength 0..1` | Blend toward `value` within `radius` of `center` (rest pose, model space). |
| `relax` | `iterations 1..=64`, `strength 0..1` | Laplacian smoothing over mesh edges. |
| `ramp` | `from`, `to` (vertex indices), `a`, `b` | `a` at `from` to `b` at `to`, by projection on the rest segment, clamped. |
| `flood` | `seed`, `tolerance >= 0`, `value` | The edge-connected region within `tolerance` of the seed's value. |
| `from_vertex_color` | `channel: "r" \| "g" \| "b" \| "a"`, `low`, `high`, `colors: [[r, g, b, a], ...]` | One color per render vertex; each sim vertex takes its nearest render vertex's channel, mapped to `[low, high]`. |

`changed` counts LOD 0 values that changed (every vertex when the map was empty: an unset `reach` starts at
1,000,000 m). `dirty: true` marks the asset as edited; the editor's save writes the maps back into the `.cloth.ron`
as dense arrays. An edit that leaves a value out of its range (`reach < 0`, `pose_pull > 1`, a fractional `layer`)
answers `F0002` with the solver reason (`cloth_bad_param`, `cloth_particle_out_of_range`, `cloth_non_finite`,
`cloth_negative` for a negative flood `tolerance`, `cloth_map_size_mismatch` for a `from_vertex_color` list that
does not match the render vertices) and changes nothing.

### Asset checks

- **The edit took:** `physics.cloth_edit_map { asset, map: "reach", edit: { op: "set", vertices: [[0, 0]] } }`, then
  `physics.cloth_inspect { target: { entity } }`: the solver was rebuilt (`steps` restarted, `state` `settling`).
- **The section draws:** `physics.cloth_inspect { target: { entity } }`: `instance.section_vertices` is above 0 and
  `failed` is empty; screenshots at `blend` 0 and 1 (`physics.cloth_set_params { patch: { blend } }`) differ.
- **Hidden cloth sleeps:** hide the cloth entity (`Visibility::Hidden`), `physics.cloth_inspect`: `state` is
  `paused` and `steps` stops growing.
- **It is drawn:** `seeing.screenshot` at `blend` 0 and 1 (`physics.cloth_set_params { patch: { blend } }`).

### Presets

| Method | Class | Params | Result |
|---|---|---|---|
| `physics.cloth_presets` | read | none | `{ presets: [{ name, substeps, iterations, damping, stretch_compliance, shear_compliance, bend_compliance, density, strain_cap, tether, wind_drag, wind_lift, gravity_scale, tear_strain }] }` |

The named starting points are `light_fabric`, `heavy_fabric`, `stiff_fabric`, `elastic`, `chain`, `silk`, `denim`,
`leather` and `rope_hemp`. Every number is an authoring choice tuned to look right within published textile ranges
(areal density in kg/m², bending rigidity as the Kawabata Evaluation System tabulates it); none is a measurement.
`physics.cloth_presets` needs no `ClothPlugin`.

`physics.cloth_set_params { target, patch: { preset: "silk" } }` applies a preset: it sets substeps, iterations,
damping, gravity scale, strain cap, tether, the aerodynamic wind model and tearing, and rewrites the stretch and bend
compliance of every link (shear compliance and `density` are fixed when the solver is built; a `.cloth.ron` or a
`ClothGrid` takes them from the preset). Other keys of the same patch are applied after the preset, so
`{ preset: "silk", damping: 1.5 }` is silk with damping 1.5. Applying the same preset again changes nothing. A soft
body refuses `preset` with `cloth_bad_param`.

`.cloth.ron` accepts `preset: Some("denim")` next to `params`: a `params` field still at its default takes the
preset's number, a field the author changed wins. An unknown name fails the cook with `cloth_asset_invalid`
(`unknown_cloth_preset` in the message).

| Code | Reason | When |
|---|---|---|
| `F0002` | `unknown_cloth_preset` | `patch.preset` names no preset; the patch changes nothing. |

- **A preset took:** `physics.cloth_set_params { patch: { preset } }`, then `physics.cloth_inspect`: `params.substeps`
  and `params.damping` equal the row from `physics.cloth_presets`.
- **Stiffness order:** pin a grid's top edge, set `physics.cloth_wind`, `physics.cloth_step 240` for `light_fabric` and
  `stiff_fabric` solvers: the stiff one's bottom edge sits lower and less downwind.

## Self-collision, garment layers and the extras

`physics.cloth_set_layers` (class `play`, ADR-2425) is registered by the same `install_cloth_methods` call.

| Method | Class | Params | Result |
|---|---|---|---|
| `physics.cloth_set_layers` | play | `target`, `layer?: 0..=255`, `particles?: [u32]` (at most 1,024) or `range?: { from, count }`, `clear?`, `layer_gap?` | `{ max_layer, layered, layer_gap }` |

- **Vertex-triangle self-collision.** `physics.cloth_set_params { patch: { self_collision: { mode: "vertex_triangle",
  radius } } }` keeps every particle `radius` away from nearby sim triangles, on the side of the triangle it was on at
  the start of the fixed step, so a folded sheet or a layered garment does not pass through itself. It needs the
  solver's sim triangles (grids and cooked assets have them). Distance LOD 1 runs it as `spheres`, LOD 2 switches it
  off. Edge-against-edge crossings are not tested.
- **Garment layers.** `physics.cloth_set_layers` puts particles on a layer (higher is farther out): by `particles`
  or by a `range` of particles (a vertex set), with `layer`. `clear: true` first puts every particle back on layer 0.
  A self-collision pair across two layers keeps the extra `layer_gap` (meters; `null` is the default, twice the
  radius) and only the outer particle moves, so inner layers push outer layers and never the reverse. Cooked assets
  set layers from their `layer` vertex map. Every check runs before the solver changes.
- `physics.cloth_inspect` adds `self_crossings` (vertex-triangle pairs that ended the last substep on the wrong side
  of their triangle; 0 when nothing crossed), `max_layer`, `layered` (particles not on layer 0), `layer_gap` (`null`
  for the default) and, at the top level, `parallel` (whether the `ClothSet` steps its solvers in parallel; hosts set
  it with `ClothSet::set_parallel`, and the result is bit-identical either way).
- **Fast colliders** (no method): a collider or snapshot shape that moves more than the collision thickness in one
  substep is tested along the particle's path relative to it, so a fast ball carries the cloth instead of passing
  through it.
- **Reaction torque** (no method): with `reaction: true` the rigid body under the cloth also receives the torque of
  the contact impulses about its center of mass (`ClothSolver::take_reaction_wrench`).

Errors carry `data.reason`: `cloth_not_found` (`F0003`), `cloth_plugin_missing` (`F0003`), `cloth_bad_param`
(`F0002`: no `particles`, `range`, `clear` or `layer_gap`; both `particles` and `range`; `layer` missing or over 255;
a negative `layer_gap`; a `clear` that is not a boolean) and `cloth_particle_out_of_range` (`F0002`: an index or a
`range` past the last particle).

<!-- depth-2:method-reference -->

## Call reference

Parameters, results and error codes for every method on this page. Types and defaults come from the schema and handlers; limits include the contract. Error codes are in `data.code`; names explicitly labeled `data.reason` are additional handler reasons. Every method can also return `F0001`, `F0002`, `F0004`, `F0007` and `F0012`. The server can return `F0005`, `F0006`, `request_too_large`, or `F0008` when the method is not registered; see [protocol](protocol.md).

Unless a lower or explicit method limit is listed, the dispatcher accepts at most 65,536 items in a top-level array parameter and 1,048,576 UTF-8 bytes in a string parameter. A limit violation returns `F0002`.

The request in each fence is the call that was sent with `regia agent call`.
The line under it is the host's answer.
A second fence tagged `json,ignore` is an error answer. Large fields are elided with `...`. A fence tagged `json,ignore` is a real call on a host or plugin method that the runtime schema does not list.


### `physics.cloth_edit_map`

Edit a vertex map (reach, standoff, pose_pull, sim_mix, mass_scale, layer) of the cloth asset every ClothSim with that asset name uses; the solvers rebuild and settle.

*EditScene, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `asset` | string | yes | the cloth asset's name (its .cloth.ron `name`) |
| `map` | string | yes | reach / standoff / pose_pull / sim_mix / mass_scale / layer |
| `edit` | object | yes | a map edit: { op: set / sphere / relax / ramp / flood / from_vertex_color, ... } (book/agents/cloth.md) |

Result: `{ changed, instances, dirty }`.

Errors besides the universal codes: `F0003`. `data.reason`: `cloth_bad_param`, `cloth_map_size_mismatch`, `cloth_negative`, `cloth_non_finite`, `cloth_not_found`, `cloth_particle_out_of_range`.

```json
physics.cloth_edit_map {"asset": "cape", "map": "reach", "edit": {"op": "set", "vertices": [[0, 0.0]]}}
```

```json,ignore
→ {
  "message": "no ClothSim uses a cloth asset named \"cape\"",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "physics.cloth_stats lists the solvers; scene queries list ClothSim entities",
    "reason": "cloth_not_found"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:physics.cloth_edit_map -->

### `physics.cloth_inspect`

Cloth and soft-body solvers in depth: cloth_stats fields plus state, blend, LOD, self-collision, tearing, wind, pose error, volume, and a particle slice.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `target` | object | no | { name } of a cloth solver or soft body; absent = every one |
| `particles` | object | no | { from, count <= 256 }: include these particles' position, velocity and inv_mass |

Omitted `particles` omits the slice. Within a supplied slice, `from` defaults to 0 and `count` to 256; count must not exceed 256.

Result: `{ plugin, parallel, solvers: [{ name, kind, ...cloth_stats fields, state, blend, lod, dropped_self_pairs, self_contacts, self_crossings, max_layer, layered, layer_gap, torn, wind, max_pose_error, teleports, tethers, model?, volume?, rest_volume?, pressure?, inverted_tets?, particles?, instance? }], failed: [{ entity, asset, error }] }`.

Errors besides the universal codes: `F0003`. `data.reason`: `cloth_bad_param`, `cloth_not_found`, `cloth_particle_out_of_range`.

```json
physics.cloth_inspect {"target": {"name": "banner"}, "particles": {"from": 0, "count": 8}}
→ { "plugin": false, "solvers": [] }
```

<!-- capture:physics.cloth_inspect -->

### `physics.cloth_pin`

Pin particles of a cloth solver (inverse mass 0).

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `target` | object | yes | { name } of a cloth solver or soft body (F0003 cloth_not_found when there is none) |
| `particles` | array | yes | particle indices (at most 1024) |

Result: `{ pinned }`.

Errors besides the universal codes: `F0003`. `data.reason`: `cloth_bad_param`, `cloth_not_found`, `cloth_particle_out_of_range`, `cloth_plugin_missing`.

```json
physics.cloth_pin {"target": {"name": "banner"}, "particles": [0, 1]}
```

```json,ignore
→ {
  "message": "this app has no cloth plugin",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "add regia_physics::cloth::ClothPlugin and call install_cloth_methods",
    "reason": "cloth_plugin_missing"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:physics.cloth_pin -->

### `physics.cloth_presets`

The named cloth starting points (light_fabric, heavy_fabric, stiff_fabric, elastic, chain, silk, denim, leather, rope_hemp) with their numbers; apply one with physics.cloth_set_params { patch: { preset } }.

*Read, does not mutate.*

No parameters.

Result: `{ presets: [{ name, substeps, iterations, damping, stretch_compliance, shear_compliance, bend_compliance, density, strain_cap, tether, wind_drag, wind_lift, gravity_scale, tear_strain }] }`.

No error codes beyond the universal set.

No parameters. The names are the `preset` values `physics.cloth_set_params` accepts.

```json
physics.cloth_presets {}
→ {
  "presets": [
    {
      "bend_compliance": 0.05000000074505806,
      "damping": 0.30000001192092896,
      "density": 0.11999999731779099,
      "gravity_scale": 1.0,
      "iterations": 1,
      "name": "light_fabric",
      "shear_compliance": 9.999999747378752e-05,
      "strain_cap": {
        "max": 1.0800000429153442,
        "min": 0.8999999761581421
      },
      "stretch_compliance": 0.0,
      "substeps": 8,
      "tear_strain": null,
      "tether": null,
      "wind_drag": 1.0,
      "wind_lift": 0.30000001192092896
    },
    {
      "bend_compliance": 0.004999999888241291,
      "damping": 0.15000000596046448,
      "density": 0.44999998807907104,
      "gravity_scale": 1.0,
      "iterations": 2,
      "name": "heavy_fabric",
      "shear_compliance": 9.999999747378752e-06,
      "strain_cap": {
        "max": 1.0499999523162842,
        "min": 0.9200000166893005
      },
      "stretch_compliance": 0.0,
      "substeps": 8,
      "tear_strain": null,
      "tether": null,
      "wind_drag": 0.800000011920929,
      "wind_lift": 0.15000000596046448
    },
    ...
  ],
  ...
}
```

Elided: `presets` (array 9).

<!-- capture:physics.cloth_presets -->

### `physics.cloth_reset`

Snap a solver to its pose (or its rest positions), zero velocities, enter settling.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `target` | object | yes | { name } of a cloth solver or soft body (F0003 cloth_not_found when there is none) |

Result: `{ state }`.

Errors besides the universal codes: `F0003`. `data.reason`: `cloth_bad_param`, `cloth_not_found`, `cloth_plugin_missing`.

```json
physics.cloth_reset {"target": {"name": "banner"}}
```

```json,ignore
→ {
  "message": "this app has no cloth plugin",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "add regia_physics::cloth::ClothPlugin and call install_cloth_methods",
    "reason": "cloth_plugin_missing"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:physics.cloth_reset -->

### `physics.cloth_set_layers`

Put cloth particles on garment layers (inner layers push outer layers, never the reverse) or change the gap kept between layers.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `target` | object | yes | { name } of a cloth solver or soft body (F0003 cloth_not_found when there is none) |
| `layer` | integer | no | layer 0..=255 for `particles` or `range` (higher is farther out) |
| `particles` | array | no | particle indices (at most 1024) |
| `range` | object | no | { from, count }: a run of particles (a vertex set) |
| `clear` | boolean | no | put every particle back on layer 0 first |
| `layer_gap` | number | no | extra distance kept across layers in meters; null restores the default (twice the self-collision radius) |

`clear` defaults to false; an omitted layer gap keeps the current one, null restores the solver default, and a number must be finite and nonnegative. Give at most one of `particles` and `range`; either requires `layer` in 0..=255.

Result: `{ max_layer, layered, layer_gap }`.

Errors besides the universal codes: `F0003`. `data.reason`: `cloth_bad_param`, `cloth_not_found`, `cloth_particle_out_of_range`, `cloth_plugin_missing`. A non-finite layer gap adds `cloth_non_finite` (`F0002`).

```json
physics.cloth_set_layers {"target": {"name": "banner"}, "range": {"from": 0, "count": 8}, "layer": 1}
```

```json,ignore
→ {
  "message": "this app has no cloth plugin",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "add regia_physics::cloth::ClothPlugin and call install_cloth_methods",
    "reason": "cloth_plugin_missing"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:physics.cloth_set_layers -->

### `physics.cloth_set_params`

Change a cloth solver's parameters (tuning, config, blend, pressure, LOD) with a patch object.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `target` | object | yes | { name } of a cloth solver or soft body (F0003 cloth_not_found when there is none) |
| `patch` | object | yes | fields to change (`preset` applies a named starting point first; unknown_cloth_preset when the name is wrong); see book/agents/cloth.md for the keys |

Omitted patch keys keep their current values; a preset applies first. Numeric values must be finite. Substeps are 1..=256 and iterations 1..=64; damping, collision thickness, pull compliance and pull scale are nonnegative. Gravity scale is -4..=4, inertia scale 0..=1, self friction 0..=2, exclude rings 0..=16, and blend 0..=1. Strain-cap `max` is at least 1 and `min` defaults to 0 (0..=1); tether is at least 1. Wind-model defaults are push gain 1, or aerodynamic drag 1 and lift 0.3. Tear defaults are strain 2 (greater than 1) and max_per_step 4 (at least 1). Air density, teleport distance and self-collision radius are positive.

Since ADR-9070 the patch also takes `gravity_override` (`[x, y, z]` or null), `stretch_compliance` and `bend_compliance` (nonnegative), `lod_distances` (ascending positive `[d1, d2, d_off]`, instances only) and `plasticity` (`{ yield, creep, max }` or null, shape-matching soft bodies only); `shear_compliance` and `friction` are refused.

Result: `{ applied: [key], params }`.

Errors besides the universal codes: `F0003`. `data.reason`: `cloth_bad_param`, `cloth_not_found`, `cloth_plugin_missing`, `unknown_cloth_preset`. Additional solver reasons: `cloth_non_finite`, `cloth_negative`, `cloth_invalid_config`, `cloth_bad_blend`.

```json
physics.cloth_set_params {"target": {"name": "banner"}, "patch": {"damping": 0.1}}
```

```json,ignore
→ {
  "message": "this app has no cloth plugin",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "add regia_physics::cloth::ClothPlugin and call install_cloth_methods",
    "reason": "cloth_plugin_missing"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:physics.cloth_set_params -->

### `physics.cloth_stats`

Cloth and rope solvers: particle and constraint counts, steps, contacts, max stretch, bounds, digest.

*Read, does not mutate.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `name` | string | no | report only the solver with this name (F0003 when there is none) |

Result: `{ plugin, solvers: [{ name, particles, pinned, distance_constraints, bend_constraints, colliders, steps, dropped_steps, contacts, max_stretch, bounds_min, bounds_max, digest }] }`.

Errors besides the universal codes: `F0003`.

With no cloth plugin the call still succeeds and `plugin` is false. A `name` that is not a solver is `F0003`.

```json
physics.cloth_stats {"name": "banner"}
→ { "plugin": false, "solvers": [] }
```

<!-- capture:physics.cloth_stats -->

### `physics.cloth_step`

Run fixed steps on one solver (or all), also while paused; returns the digest.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `target` | object | no | { name } of a cloth solver or soft body; absent = every one |
| `count` | number | yes | fixed steps, 1..=600 |

Result: `{ steps_run, digest }`.

Errors besides the universal codes: `F0003`. `data.reason`: `cloth_bad_param`, `cloth_not_found`, `cloth_plugin_missing`, `cloth_step_count_too_large`.

```json
physics.cloth_step {"target": {"name": "banner"}, "count": 60}
```

```json,ignore
→ {
  "message": "this app has no cloth plugin",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "add regia_physics::cloth::ClothPlugin and call install_cloth_methods",
    "reason": "cloth_plugin_missing"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:physics.cloth_step -->

### `physics.cloth_unpin`

Free pinned particles of a cloth solver.

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `target` | object | yes | { name } of a cloth solver or soft body (F0003 cloth_not_found when there is none) |
| `particles` | array | yes | particle indices (at most 1024) |
| `inv_mass` | number | no | inverse mass to give them (positive; default: the mean of the solver's free particles, or 1) |

Result: `{ unpinned, pinned }`.

Errors besides the universal codes: `F0003`. `data.reason`: `cloth_bad_param`, `cloth_non_finite`, `cloth_not_found`, `cloth_particle_out_of_range`, `cloth_plugin_missing`.

```json
physics.cloth_unpin {"target": {"name": "banner"}, "particles": [0, 1], "inv_mass": 1.0}
```

```json,ignore
→ {
  "message": "this app has no cloth plugin",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "add regia_physics::cloth::ClothPlugin and call install_cloth_methods",
    "reason": "cloth_plugin_missing"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:physics.cloth_unpin -->

### `physics.cloth_wind`

Set the wind of one solver (target) or the global cloth wind (no target).

*Play, mutating.*

| Param | Type | Required | Default and limit |
|---|---|---|---|
| `target` | object | no | { name } of a cloth solver or soft body; absent = every one |
| `velocity` | array | no | [x, y, z] in m/s (required unless clear) |
| `gust` | object | no | { amplitude 0..1, period_steps >= 1, seed } (target only) |
| `clear` | boolean | no | true: zero the wind and drop the gust |

`clear` defaults to false. It zeroes velocity; omit `gust` as well to drop the gust. A gust requires a target. A supplied gust requires amplitude in 0..=1 and period_steps at least 1; seed defaults to 0. With clear false and no gust, the existing gust is retained.

Result: `{ wind }`.

Errors besides the universal codes: `F0003`. `data.reason`: `cloth_bad_param`, `cloth_non_finite`, `cloth_not_found`, `cloth_plugin_missing`.

```json
physics.cloth_wind {"target": {"name": "banner"}, "velocity": [3.0, 0.0, 0.0]}
```

```json,ignore
→ {
  "message": "this app has no cloth plugin",
  "code": -32001,
  "data": {
    "code": "F0003",
    "hint": "add regia_physics::cloth::ClothPlugin and call install_cloth_methods",
    "reason": "cloth_plugin_missing"
  }
}
```

The host returned this error. `code` is the JSON-RPC code and `data` is the protocol object.

<!-- capture:physics.cloth_wind -->

## Workflow: pin a banner, set a wind, read the digest

1. `physics.cloth_presets` for the named starting points, then `physics.cloth_stats` to see which solvers exist. `plugin` false means this app has no cloth plugin (`cloth_plugin_missing` on the calls that require one).
2. `physics.cloth_set_params` with `target` `{ name }` and `patch` `{ preset }` (or the numeric keys). An unknown preset returns `F0002` with reason `unknown_cloth_preset`.
3. `physics.cloth_pin` the particle indices that should stay put (at most 1024).
4. `physics.cloth_wind` with the same target and a `velocity` array. `clear` true zeros it.
5. `physics.cloth_step` with `count` (1..=600). The answer's `digest` is the determinism check.
6. `physics.cloth_inspect` with a small `particles` window to read positions. `physics.cloth_reset` snaps the solver back to its pose when you want the same start again.

<!-- /depth-2:method-reference -->

### Layer checks

- **The layers hold:** `physics.cloth_set_layers { target, range: { from: <first outer particle>, count }, layer: 1 }`,
  `physics.cloth_step 300`, `physics.cloth_inspect`: `self_crossings` is 0 and `max_layer` is 1.
- **Nothing crosses:** with `self_collision` `vertex_triangle`, `play.wait_until` on `self_crossings == 0` after a
  fold, and `dropped_self_pairs` stays 0.
- **It is still deterministic:** `physics.cloth_reset`, `physics.cloth_step N`, compare `digest` across runs, with
  `parallel` true or false.

## Solver health and the runaway guard

`physics.cloth_health` (class `read`, ADR-3273) is registered by the same `install_cloth_methods` call. It reports
each solver's runaway guard: how far its links stretch, how often it had to work harder, and every fault the guard
caught.

| Method | Class | Params | Result |
|---|---|---|---|
| `physics.cloth_health` | read | `target?` | `{ plugin, solvers: [{ name, kind, guard: { enabled, max_strain, stiff_limit, extra_iterations, stiff_target }, max_strain, peak_strain, faults, non_finite_faults, runaway_faults, pose_resets, extra_iterations, stiff_projections, last_fault }] }` |

- `max_strain` is the largest length-over-rest ratio of an active zero-compliance (inextensible) link at the end of
  the last fixed step; `peak_strain` the largest one any clean step ended with. Compliant links are never counted: an
  elastic cloth may stretch past any ratio without a fault. Both are `null` if a length overflowed.
- `extra_iterations` counts the solver iterations the guard added because a zero-compliance (inextensible) link was
  stretched past `guard.stiff_limit` (default 1.07; none by default, `ClothGuard::extra_iterations` turns them on);
  `stiff_projections` counts the substeps that ended over it and were pulled back to `stiff_target` (1.02). A cloth that converges with its authored `iterations` keeps both at 0 and steps exactly as it
  would without the guard. Steadily growing counts mean the cloth needs more substeps or iterations for its size and
  weight (see below); the guard keeps it stable but spends time doing so.
- `faults` counts fixed steps the guard rejected: `non_finite_faults` (a NaN or infinite position or velocity) and
  `runaway_faults` (a zero-compliance link longer than `guard.max_strain`, default 10, times its
  rest length, whose own baseline — the last good free particles against its pins' current places —
  is not over the limit: a link a pin game code parked far away already stretched past the limit
  stays excused, an unrelated link over the limit still faults). A faulted step is
  undone: the solver goes back to the last step that passed, with zero velocity, or, when the step before also
  faulted or the cloth was reset since, to its rest or skinned pose (`pose_resets`). Pinned particles keep where game
  code moved them unless their places still stretch a stiff link past `max_strain`, and a pin moved far (a teleport)
  is no fault: the cloth follows it. Tearing, when enabled, runs only after the guard passed, so a faulted step
  never leaves the cloth torn. The faulted step's two-way reaction impulses are dropped. Other solvers are never
  touched.
- `last_fault` is `null` or `{ step, code, recovery, particle, strain }`: `code` is `cloth_fault_non_finite` or
  `cloth_fault_runaway`, `recovery` is `last_good` or `rest_pose`, `particle` the first non-finite particle (or
  `null`), `strain` the largest finite zero-compliance link ratio when the step ended.
- Without a plugin it answers `{ plugin: false, solvers: [] }`; an unknown `target` answers `F0003` with reason
  `cloth_not_found`, a malformed one `F0002` with `cloth_bad_param`.

Game code sets the guard with `ClothSolver::set_guard(ClothGuard { .. })` (`stiff_limit: None` turns the prevention
off, `enabled: false` the fault detection) and drains fault events with `ClothSolver::drain_faults` (at most 16 are
kept between drains; the counters keep counting). A rejected setting answers `cloth_bad_param` and changes nothing.

| Code | Meaning |
|---|---|
| `cloth_fault_non_finite` | A fixed step ended with a NaN or infinite particle; the solver was put back. |
| `cloth_fault_runaway` | A fixed step ended with a zero-compliance link longer than `max_strain` times its rest length (no moved-pin excuse); the solver was put back. |

### Why the guard works harder on big sheets

One Gauss-Seidel sweep per substep (`iterations: 1`) cannot carry the weight of a tall or heavy sheet: its
inextensible links stay stretched, and the sweep's order-dependent leftover next to pinned particles turns into speed.
A 32 x 32 sheet at 0.03 m with 4 substeps used to stretch to 1.9x and blow up; now it stays under 1.08x. The load
number `rows * 9.81 * h² / spacing` (`h` = time step / substeps) says how hard a sheet is: up to about 3 the guard keeps
its inextensible links under 1.1x; above it (one substep with 32 rows at 1 cm, or 64 rows at 3 cm) they stretch further
and the sheet needs more substeps. The compliant shear diagonals are left alone and can shear past 1.1x on very large
sheets (3 to 6 m at 4 substeps). ADR-3273 has the measured matrix.

### Health checks

- **It is healthy:** `physics.cloth_step 600`, then `physics.cloth_health`: `faults` is 0 and `max_strain` is under
  1.1 for stiff cloth.
- **It needs more substeps:** `stiff_projections` (or, with extra iterations on, `extra_iterations`) grows every
  step: raise `substeps` with `physics.cloth_set_params` until it stays near 0.
- **A fault was caught:** `play.wait_until` on `faults > 0` in a stress scene, then check `last_fault.code` and that
  `max_strain` is back under `guard.max_strain`.

## Scene wind on cloth

The weather's scene wind (`regia_scene::weather::SceneWind`, see [Weather](weather.md)) reaches every cloth placed
through `ClothSim` (ADR-2884). Each fixed step, before the wind gather, `feed_scene_wind_to_cloth` (in
`ClothGluePlugin`) sets each such solver's global wind term to `ClothSet::global_wind()` (what `physics.cloth_wind`
without a target sets) plus the scene wind `velocity3()` (gusts included) times the cloth's `wind_scale`.

- `wind_scale` is a `ClothParams` field (0..=10, default 1): optional in a `.cloth.ron` (`params: (wind_scale: 0.5)`),
  absent means 1 and 1 is never written, so older files and their cooked bytes are unchanged. A `ClothSim.params`
  override carries its own. 0 keeps a cloth out of the scene wind.
- Without a `SceneWind`, in `Off` mode, or with zero wind, a cloth steps bit for bit as before the bridge.
- Soft bodies and solvers no `ClothSim` owns (a host's own, one an agent made) do not take the scene wind.

`physics.cloth_inspect` shows it: a top-level `scene_wind` (`null` without the resource, else `{ mode: "weather" |
"manual" | "off", velocity: [x, y, z], cloth: [x, y, z] | null }`, `cloth` being what cloths get before their
scale), each solver's `global_wind` (the term it steps with), and per instance `instance.scene_wind`, the wind that
cloth received after its `wind_scale`.

### Scene wind checks

- **The wind arrives:** set a `SceneWind` (`Manual`, e.g. `[0, 5]`), step, `physics.cloth_inspect`:
  `instance.scene_wind` is `[0, 0, 5]` times the cloth's `wind_scale` and the bounds move downwind.
- **Calm is today's cloth:** with `SceneWind::CALM` (or none) record `digest` after N steps; it equals a run without
  the resource.

## Editor: cloth inspector and paint mode

The editor's **Cloth** panel (spec 55 § 6, `regia_editor::cloth_panel`) edits one `.cloth.ron` or `.soft.ron` of the
project through its own types and writes it atomically; every change is one step on the editor's undo stack (a slider
scrub on one field is one step, a paint stroke is one step). An unedited file is never rewritten, and a file edited
back to how it was opened is written with its original bytes (comments and layout included). Other saves write the
canonical text.

- **Inspector:** every `ClothParams` number (`substeps`, `iterations`, `damping`, `gravity_scale`,
  `stretch_compliance`, `shear_compliance`, `bend_compliance`, `pull_compliance`, `inertia_scale`,
  `collision_thickness`, `friction`, `self_friction`, `air_density`, `teleport_distance`, `density`, `wind_scale`),
  the tether, the starting point (`preset`, the names `physics.cloth_presets` lists), the self-collision mode and the
  wind model. The target instance's `blend` and `lod_distances` live on its `ClothSimSource` (saved with the level)
  and change through the same undo stack. Opening a file binds the one instance whose source names that same file
  (`cloth/flag.cloth.ron` and `assets/cloth/flag.cloth.ron` match; `armor/cloth/flag.cloth.ron` does not); when
  several instances name it, none is bound and instance edits answer `cloth_panel_no_target` until `entity` picks
  one. A soft body gets the subset it has: `substeps`, `iterations`, `damping`,
  `gravity_scale`, `collision_thickness`, `particle_mass`, `edge_compliance` and its model's `pressure`,
  `volume_compliance`, `stiffness`, `beta`, plus the instance's `lod_distances`.
- **Paint:** a brush on the sim mesh (map picker, value, radius, falloff, strength) issues § 3.4 `sphere` edits along
  a stroke, dabs spaced `radius / 4` apart (or `spacing`), through `regia_agent::cloth::edit_cloth_map`, the code
  `physics.cloth_edit_map` runs. The sim mesh is the asset of a `ClothSim` with the file's name (those instances get
  the paint at once), else the file cooked from its glTF. The viewport colors each sim vertex on a ramp in map units
  (meters for `reach` and `standoff`); Ctrl-click selects a vertex. The simulate preview steps a private solver of the
  asset with the edited numbers; the level is not touched.
- **Gizmos:** the `cloth` group, off by default: body proxies, the target solver's world-snapshot shapes, and for each
  selected vertex a `reach` sphere (capped at 10 m) and a `standoff` plane through `p - n * standoff`.

**New files** (w11 forms-undo-autosave, ADR-7731): Content ▸ New ▸ Cloth and ▸ Soft body (`editor.create_asset {
kind: "cloth" | "soft_body", path }`) write a `.cloth.ron` / `.soft.ron` named after the file, at the current
version, with default parameters (a soft body is a pressure model at rest volume, incompressible). The mesh is node
`<name>` of `<name>.glb` beside the file: export the sim mesh (a closed surface for a soft body) there, or point
the file's `sim_mesh` / `surface` at another glTF. Unsaved edits in this panel are autosaved for crash recovery (kind `cloth`,
[editor autosave](editor-autosave.md)).

| Method | Class | Params | Result |
|---|---|---|---|
| `editor.cloth_panel_state` | read | `entity?` | `{ open, path, dirty, source: { kind, name, preset?, params, tether?, self_collision?, wind_model?, maps }, sim_mesh, sim_error, instance, paint, simulate, preview, gizmos, selected, gizmo_shapes: { proxy, snapshot, reach, standoff }, undo_depth, redo_depth, error }` |
| `editor.cloth_panel_edit` | assets | `path?`, `entity?`, `params?: { name: number }`, `preset?`, `tether?`, `self_collision?: { mode, radius }`, `wind_model?: { mode, drag, lift, gain }`, `blend?`, `lod_distances?`, `gizmos?`, `selected?`, `paint?: { on, map, value, radius, falloff, strength, spacing, ramp }`, `simulate?`, `preview_steps?: 1..=600`, `undo?`, `redo?`, `save?` | the state plus `written` |
| `editor.cloth_paint_stroke` | assets | `points: [[x, y, z]]` (model space, rest pose), `path?`, `entity?`, `map?`, `value?`, `radius?`, `falloff?`, `strength?`, `spacing?`, `values?` | `{ dabs, changed, instances, map, undo_depth, values? }` |

`editor.cloth_panel_edit` applies the file changes all or nothing (one bad name refuses the call), then the instance
fields, the brush, gizmos, selection, the preview, undo / redo and the save, in that order.

Errors carry `data.reason` (`F0002`, or `F0003` for the `_not_found` ones): `cloth_panel_no_project`,
`cloth_panel_bad_path` (not a relative `.cloth.ron` / `.soft.ron` inside the project), `cloth_panel_io_failed`,
`cloth_panel_parse_failed`, `cloth_panel_serialize_failed`, `cloth_panel_no_doc` (nothing open), `cloth_panel_bad_value`
(out of range, not finite, a whole number expected, a stroke with no points or past 4,096 points or 16,384 dabs, more
than 1,024 selected vertices, a vertex past the mesh, `preview_steps` outside 1..=600), `cloth_panel_unknown_param` (a
name the file kind does not have, a cloth setting on a soft body), `cloth_panel_unknown_preset`,
`cloth_panel_no_sim_mesh` (paint or preview on a soft body, or no `ClothSim` and the glTF did not cook),
`cloth_panel_no_target` (no instance picked, an `entity` that does not exist, blend on a soft body), and the paint
edit's own reasons from `physics.cloth_edit_map` (`cloth_bad_param`, `cloth_particle_out_of_range`).

### Panel checks

- **One field, one line:** `editor.cloth_panel_edit { path, params: { damping: 1.25 }, save: true }`; the file differs
  from the original in the `damping` line only.
- **Paint equals the headless edit:** `editor.cloth_paint_stroke { points, map, value, radius, falloff, strength }`,
  then replay its `dabs` as `physics.cloth_edit_map` `sphere` edits on another instance: the maps are equal.
- **Undo restores the file:** edit, save, `{ undo: true }` per step, `{ save: true }`: the file has its original bytes.
- **Gizmos:** `{ gizmos: true, selected: [i, j, k] }`: `gizmo_shapes.reach` and `gizmo_shapes.standoff` are 3; with
  `gizmos: false` (the default) every count is 0.
