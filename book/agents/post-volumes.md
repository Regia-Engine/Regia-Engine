# Post volumes and the physical camera (agents)

A `PostVolume` component changes the post settings of every camera whose eye it reaches. A
`PhysicalCamera` lens can take the camera's exposure from ISO, shutter time and aperture. Design and
the exact blending rules: `docs/specs/30-post-volumes-and-physical-camera.md` (ADR-1464). The user
guide page is [Post volumes and the physical camera](../user-guide/post-volumes.md).

## Methods

| Method | Permission | What it does |
|---|---|---|
| `render.post_volumes { camera?, point? }` | read | Every volume, the ones that reach the eye with their weights, the blended values there. |
| `render.set_post_volume { entity, ... }` | edit scene | Adds a volume to an entity or edits it. |
| `render.get_physical_camera { entity? }` | read | A camera's lens and what it gives. |
| `render.set_physical_camera { entity?, ... }` | edit scene | Sets a camera's lens, inserting `PhysicalCamera` when absent. |
| `render.set_post_volume_luts { enabled?, reload? }` | edit scene | Turns the per-volume LUT blend on or off (off by default); reports the loaded tables. |
| `render.post_volume_gizmos { entity? }` | read | The editor gizmo's data: each volume's shape and band outline as world line segments. |

`camera` and `entity` default to the active window camera (the window camera with the lowest
`order`, the one the renderer draws first).

### `render.post_volumes`

Params: `camera` (entity id) or `point` (`[x, y, z]`), not both. Result:

- `camera`, `eye`: where the volumes were weighed.
- `volumes`: every volume sorted by entity id: `entity`, `format_version`, `supported`, `enabled`,
  `shape` (`{ kind: "box", half_extents }`, `{ kind: "sphere", radius }`, `{ kind: "convex_hull",
  planes, valid }` or `{ kind: "unbound" }`), `priority`, `weight`, `blend_distance`, `overrides` (every
  field, `null` when not set), `sets` (the names of the fields it sets), `lut` (the LUT path or `null`)
  and `weight_here` (its effective weight at the eye, 0–1).
- `contributing`: the volumes with a positive weight at the eye, in the order they are applied
  (priority, then entity id): `{ entity, priority, weight }`.
- `resolved`: the value of every overridable field at the eye after the blend, starting from the
  project's post settings and the level look (the camera's EV pin and its lens are not included).
  Local exposure fields are `null` when local exposure is off.
- `eye_ranges`: the contributing volumes that ask the eye for an EV range. The eye itself is in
  `render.get_exposure`.
- `lut_blend` (format v3): `enabled` (the switch), `paths` (every LUT path seen, `{ path, loaded }`),
  `stats` (`loads`, `failures`, `blends`, `cache_hits`), `base_lut` (the level's LUT, `{ size, key }` or
  `null`), `volumes` (the volumes naming a LUT, with `weight`, `priority`, `path`, `loaded`; empty while
  off) and `resolved_lut` (the view's LUT at the eye after the blend, `{ size, key }` or `null` for the
  identity). Computed without touching the renderer's cache.

### `render.set_post_volume`

Params (every one but `entity` optional; all are validated before anything changes):

| Param | Value |
|---|---|
| `entity` | Entity id. It gets the default volume (a 1 m box, priority 0, weight 1, no overrides) when it has none, and a transform at the origin when it has none. |
| `enabled` | Boolean. |
| `shape` | `{ kind: "box", half_extents: [x, y, z] }`, `{ kind: "sphere", radius }`, `{ kind: "convex_hull", planes: [[nx, ny, nz, d], ...] }`, `{ kind: "unbound" }` or the string `"unbound"`. Sizes 0–100 000 m. A hull has 1–16 planes, inside where `n · x <= d` in local space, each value within ±100 000 and each normal non-zero (format v3). |
| `priority` | 32-bit integer. |
| `weight` | 0–1. |
| `blend_distance` | Meters, 0–100 000 (0 is a hard edge). |
| `overrides` | Partial object of the fields below; `null` clears a field, a field left out keeps its value. |
| `lut` | A `.cube` or PNG-strip `.png` path, at most 1024 bytes, no NUL and no `..` component; `null` clears it (format v3). `\` is accepted as a separator. An absolute path is opened as that path. A relative `.cube` loads from the asset server (paks or the cooked cache; a packaged Android client has nothing else), then a loose file under the project root. A relative `.png` loads only as a loose file. It blends in only while `render.set_post_volume_luts` has it on. |

Override fields and accepted values:

| Field | Value |
|---|---|
| `exposure_compensation` | EV, −16..16 |
| `exposure_min_ev100`, `exposure_max_ev100` | EV100, −20..30 (the auto-exposure band) |
| `meter_center_weight` | 0–1 (0 averages the frame, 1 weights its centre) |
| `tonemap` | `agx`, `aces_fitted`, `neutral` |
| `contrast` | 0.05–8 |
| `saturation` | 0–8 |
| `bloom_intensity` | 0–1 |
| `white_balance_kelvin` | 1667–25 000 |
| `tint` | −1..1 |
| `lift`, `gamma`, `gain` | `[r, g, b]` |
| `vignette`, `film_grain` | 0–1 |
| `local_shadow_contrast`, `local_highlight_contrast` | 0–1 |
| `local_detail_strength` | 0–4 |
| `ao_intensity` | 0–4 (scales the screen-space AO; 1 unchanged) |
| `fog_tint` | `[r, g, b]` multiplier on the view's fog colour |
| `eye` | `{ ev_min, ev_max, adapt_up_speed?, adapt_down_speed? }` (EV100 −20..30, `ev_min <= ev_max`, speeds 0–100 EV/s, default 2.0 up and 1.5 down) |
| `lens_flare_intensity` | 0–1000, the ghost flare strength (format v2; [lens effects](lens-effects.md)) |
| `lens_flare_threshold` | 0–65 504, the exposed luminance (1 = display white) that casts ghosts (format v2) |
| `lens_dirt_intensity` | 0–10, the lens dirt strength (format v2) |
| `chromatic_aberration` | 0–0.05, lateral colour as a fraction of the radius (format v2) |

Result: `{ entity, created, before, after }` (`before` is `null` when the volume was created).

### `render.get_physical_camera`

Result: `{ entity, has_lens, lens, derived, exposure_modes, depth_of_field_modes }`. Without a lens
the defaults are shown and `has_lens` is `false`.

- `lens`: `exposure_mode` (`inherit`, `manual`, `physical`), `manual_ev100` (`null` unless manual),
  `iso`, `shutter_seconds`, `shutter_reference_fps`, `aperture` (f-stops), `focal_length_mm` (`null`
  keeps the camera's field of view), `sensor_height_mm`, `focus_distance` (meters),
  `depth_of_field` (`off`, `gaussian`, `bokeh`), `shutter_angle`, `motion_blur_samples`.
- `derived`: `ev100` (the lens's EV100, `log2(N² / t) − log2(ISO / 100)`), `view_ev100` (the EV100
  the view uses: the manual EV, the lens's EV100 in physical mode, `null` for inherit),
  `shutter_fraction` (the motion blur's strength) and `vertical_fov_deg` (from the focal length, or
  `null`).

### `render.set_physical_camera`

Params: `entity` and any of the `lens` fields above (`exposure_mode`, `manual_ev100`, `iso`,
`shutter_seconds`, `shutter_reference_fps`, `aperture`, `focal_length_mm`, `sensor_height_mm`,
`focus_distance`, `depth_of_field`, `shutter_angle`, `motion_blur_samples`). `exposure_mode: manual`
needs `manual_ev100` unless the lens is already manual; `manual_ev100` alone retunes a manual lens.
`focal_length_mm: null` clears the focal length. Result: `{ entity, created, before, after, derived }`.

In `physical` mode the shutter time also sets the motion blur (`shutter_seconds ×
shutter_reference_fps` of a frame), and the aperture is the depth of field's too. A lens in
`manual` or `physical` mode wins over the camera's `exposure_ev` pin; `inherit` leaves the pin and
auto exposure in charge.

### `render.set_post_volume_luts`

Params: `enabled` (boolean) and `reload` (boolean: forget the loaded tables and blends so the next
frame reads the files again, after a LUT file changed). Both optional; inserts the switch when the
world has none. Result: `{ before, after }`, each `{ enabled, paths: [{ path, loaded }], stats }`.

While on, every frame each camera blends the LUTs of the volumes at its eye over the level's LUT with
the scalar rules (tiers by priority, weight-average inside a tier, `α = min(1, Σ w)`), and the post
stack's colour grade samples the result. Off (the default) leaves every frame exactly as it was.

### `render.post_volume_gizmos`

Params: `entity` (optional, one volume). Result: `{ volumes: [{ entity, enabled, supported, priority,
shape, blend_distance, outline }] }` sorted by entity. `outline` is `{ shape, band, bounded }`: `shape`
and `band` are arrays of world-space segments `[[x, y, z], [x, y, z]]` (the band is empty without a
blend distance), `bounded` is false for a hull with rays (a half-space, a slab, a wedge, an open prism),
which are not drawn. An unbound volume and an invalid hull have `outline: null`.

## Errors

Codes are `F0002` (bad params) and `F0003` (missing camera or entity); `data.reason` says which:

| Reason | Code | When |
|---|---|---|
| `camera_missing` | F0003 | No window camera and no `camera` / `point` / `entity` given. |
| `not_a_camera` | F0003 | The entity has no `Camera` (or, for `render.post_volumes`, no `GlobalTransform`). |
| `entity_missing` | F0003 | `render.set_post_volume` or `render.post_volume_gizmos` names an entity that does not exist. |
| `camera_and_point` | F0002 | `render.post_volumes` got both `camera` and `point`. |
| `invalid_point` | F0002 | `point` is not three finite numbers. |
| `invalid_shape` | F0002 | `shape` is not one of the four forms, a size is out of range, or a hull has 0 or more than 16 planes, a plane that is not 4 numbers, or a zero normal. |
| `unknown_field` | F0002 | A param the method does not take. |
| `unknown_override` | F0002 | An `overrides` key that is not an override field. |
| `invalid_override` | F0002 | An override value of the wrong type or range (or an `eye` with `ev_min > ev_max`). |
| `invalid_value` | F0002 | Any other param of the wrong type or range. |
| `invalid_mode` | F0002 | `exposure_mode` or `depth_of_field` is not a known name. |
| `manual_needs_ev` | F0002 | `manual` without `manual_ev100`, or `manual_ev100` on a lens that is not manual. |
| `newer_format` | F0002 | The entity's volume was written by a newer engine (`format_version` above this engine's); it is not edited. |
| `invalid_lut` | F0002 | `lut` is not a `.cube` / `.png` path string of 1–1024 bytes without `..`. |
| `not_a_volume` | F0003 | `render.post_volume_gizmos` names an entity without a `PostVolume`. |

## Example: check an indoor-outdoor transition

```json
{"method":"render.set_post_volume","params":{"entity":4294967310,
  "shape":{"kind":"box","half_extents":[6,3,10]},"blend_distance":3,"priority":1,
  "overrides":{"exposure_compensation":1.0,"white_balance_kelvin":9000,"saturation":0.9,
               "eye":{"ev_min":6,"ev_max":8}}}}
{"method":"render.post_volumes","params":{"point":[0,1.6,-11.5]}}
```

At 11.5 m along Z the point is 1.5 m outside the 10 m half length, half way through the 3 m band:
the volume's `weight_here` is 0.5 and `resolved.exposure_compensation` is half way to 1.0. Move the
camera through the door with `play.*` and wait on `exposure_settled` (`book/agents/exposure-zones.md`)
before a screenshot.

## Example: a slanted hall with its own LUT

```json
{"method":"render.set_post_volume_luts","params":{"enabled":true}}
{"method":"render.set_post_volume","params":{"entity":4294967311,"priority":2,"blend_distance":1.5,
  "shape":{"kind":"convex_hull","planes":[[1,0,0,4],[-1,0,0,4],[0,1,0,3],[0,-1,0,0],[0,0,1,12],[0,0,-1,12],
                                          [0.7071,0.7071,0,4.5]]},
  "lut":"looks/hall.cube","overrides":{"exposure_compensation":0.75}}}
{"method":"render.post_volumes","params":{"point":[0,1.6,0]}}
{"method":"render.post_volume_gizmos","params":{"entity":4294967311}}
```

Inside the hall `lut_blend.resolved_lut.key` is the hall table's key (a full-weight single volume hands
its table over); 0.75 m outside a face it is a new key, the half-way blend with the level's LUT.

## Saved lenses and the volume overlay (ADR-2323)

- `PhysicalCamera` is a reflected scene component (`regia_render::lens::PhysicalCamera`, format `physical-camera`
  v1): a lens set with `render.set_physical_camera` (or the inspector) is saved with the level and comes back on load.
  A lens written by a newer engine is ignored by the renderer (logged once), and the camera renders as if it had none.
- The debug gizmo group `post_volumes` draws every enabled volume: its shape in a color keyed by priority and the
  outer edge of its blend band dimmer; unbound volumes draw nothing. Turn it on with
  `debug.set_gizmos { group: "post_volumes", enabled: true }` before a `seeing.screenshot`.
- The overlay draws exactly the segments `render.post_volume_gizmos` returns (`regia_render::post_volume_outline`):
  boxes, spheres and convex hulls alike, the shape first and then the band. A hull with rays (an unbounded hull)
  draws only its bounded edges (`bounded: false` in the answer). A disabled volume, a volume from a newer engine, a
  box with a zero extent, a sphere with a zero radius and an invalid hull draw nothing, though
  `render.post_volume_gizmos` still outlines a disabled one (the editor greys it).
- `regia upgrade-project` lists the post volumes and lenses inside each `.level.ron`, `.prefab.ron` and level
  directory (`.level.d`, one row per format under the directory's path) as their own rows (format `post-volume`
  or `physical-camera`, the lowest version in the file, or the highest when one is newer than the engine) and
  `--write` rewrites them at the current version after the file (or directory) itself, with one `.bak` of the
  original. The renderer migrates old components at load anyway; the rewrite only makes the files current.

### Example: overlay and gizmo data agree

```json
{"method":"render.set_post_volume","params":{"entity":4294967311,"blend_distance":0.5,
  "shape":{"kind":"convex_hull","planes":[[1,0,0,1],[-1,0,0,1],[0,1,0,2],[0,-1,0,2],[0,0,1,3],[0,0,-1,3]]}}}
{"method":"render.post_volume_gizmos","params":{"entity":4294967311}}
{"method":"debug.set_gizmos","params":{"group":"post_volumes","enabled":true}}
{"method":"seeing.screenshot","params":{}}
```

The hull is a 2 x 4 x 6 m box: `volumes[0].outline` lists 12 `shape` and 12 `band` segments with `bounded: true`,
and the screenshot shows the same 24 lines, the band 0.5 m outside each face.

## Sample: a walk into a hull volume

`cargo run -p render_lab -- post_volumes --out target/render-lab` renders five headless frames of a camera walking
into a wedge-shaped convex-hull volume (a slanted roof, a 2 m band, a warm LUT and a saturation override) and
prints one line per step: `post_volumes: step <i> eye_z <z> weight <w> mean_red <r> mean_blue <b>`. The weight
is 0 outside the band and 1 inside, and the frames warm with it; `samples/render_lab/tests/post_volumes_walk.rs`
checks exactly that, comparing the frames with each other (no golden image).
