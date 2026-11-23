# Forest flythrough

Use these methods to fly a camera through a dense forest: find the trees along a planned path, measure how much room the
camera has, carve a corridor where it has too little, and switch on the LOD and sway settings a fast camera needs
(ADR-1503). Everything is deterministic: the same set, terrain and path give the same answers, and a scripted flight
renders the same draws on every run.

The host inserts `regia_terrain::scatter::forest::ForestFlight::with_query(query, Some(library), key_base)` next to its
`RuntimeScatter` (`query` is a `ScatterQuery` over the same set and terrain as the field, `library` and `key_base` are
the field's), and adds `ForestFlightPlugin` (the showcase host inserts it without the library: carves are reported, not
installed). Without the resource every method answers `F0003`. Bad parameters are
`F0002`. An unknown class name is `F0003`.

## `terrain.forest_query`

Scatter instances straight from placement, so nothing has to be streamed in (a path compiler can ask about a flight
kilometres ahead of the camera).

- `{ "box_min": [x, y, z], "box_max": [x, y, z] }`: instances whose ground point lies in the box's XZ extent and whose
  base-to-top span overlaps its Y range. At most 4 km² per call.
- `{ "from": [x, y, z], "to": [x, y, z], "radius": 10 }`: instances whose surface comes within `radius` metres of the
  segment, with `distance` (negative: the segment passes through it), `part` (`trunk` or `crown`) and `t` (0..1 along
  the segment). At most 5 km per call.
- `classes` (names) filters, `limit` caps the hits (default 1000, at most 20000).

→ `{ count, truncated, hits: [{ class, mesh, id, position, height, radius, trunk_radius, crown_base_y, crown_radius,
top_y, ... }] }`. `id` is stable for an instance across calls and runs (it is not the renderer's object key).

Each instance is a trunk capsule under a crown spheroid, measured from its mesh: the trunk radius is the widest point
of the lowest fifth of the mesh, the crown begins where the mesh is 2.5 times wider than the trunk, and the crown
distance is a lower bound (a clearance it reports is a clearance the camera has). A mesh that never widens (a rock) is
all trunk.

## `terrain.forest_corridor`

`{ "points": [[x, y, z], ...], "times": [s, ...]?, "clearance_m": 2, "ground_clearance_m": 2, "search_m": 12,
"classes": [...]?, "carve": false }` → `{ report, carve? }`.

`report.segments[i]` (one per path segment): `clearance_m` (smallest clearance to any trunk or crown, `null` when
nothing is within `search_m`), `nearest` (`class`, `id`, `part`, `position`, `height`, `distance`), `violations`
(instances closer than `clearance_m`), `ground_clearance_m`, `terrain_violation`, `canopy_cover` (share of samples
with a crown overhead: low values are gaps to burst through), `suggested_lift_m` (lift that clears the ground),
`speed_m_s` (with `times`), `ok`. The report also sums `violating_segments`, `obstacle_violations` (distinct
instances), `terrain_violations`, `min_clearance_m`, `min_ground_clearance_m`, `canopy_cover` and `length_m`.

With `carve: true`, `carve` lists per class the exclusions that would clear every obstacle violation, without
installing them.

## `terrain.forest_carve`

The same parameters (no `carve`) → `{ before, carve, exclusions_added, removed, after, scatter_ron, installed }`.

Per class, every run of path pieces with violators becomes one `.scatter.ron` exclusion along the path (simplified to
0.25 m) whose radius reaches the farthest violator's ground point. Exclusions are a field of the existing v2 format,
and placement only drops the candidates an exclusion covers, so no other tree moves and `scatter_ron` loads in every
build. The carved set is installed into the running field on the next frame (the field is rebuilt with the same
impostor, detail and motion settings). `removed` counts violators plus the trees that share their stretch of
corridor. Terrain violations cannot be carved: lift the path by `suggested_lift_m`.

## `terrain.forest_flight`

`{}` → `{ motion, sway, wake, fps, meter_pops, pop_threshold_px, has_query, can_install, pending_carve, stats }`.
`stats` holds the last frame's sway (`swaying`, `started`, `settled`, `max_tilt_mrad`), `leaned` draws, pops
(`pops_frame`, `pops_total`: `hard_switches`, `pop_ins`, `vanishes`, `soft_switches`, `max_switch_px`) and
`rebuilds` / `last_error` of carve installs.

With `{ "speed_m_s": 100, "fps": 60, "height_px": 2160, "fov_deg": 50 }` it adds `check`, every class against that
speed: `fade_band_m`, `band_crossing_s` (head-on), `prefetch_lead_s` (one tile side of background prefetch),
`lod_fade_travel_m` and `recommended_fade_band_m`, with warning codes:

| code | meaning | fix |
|---|---|---|
| `fade_band_short` | the fade or impostor band takes under 0.25 s to cross, so the dissolve reads as a pop | widen the class's `fade_start..fade_end` to `recommended_fade_band_m` |
| `prefetch_lead_short` | the background tile prefetch is less than 0.5 s ahead, so needed tiles are placed on the frame | path prefetch (continuous-flight lane), or a slower pass |
| `lod0_switch_near` | the class leaves LOD 0 less than half a second ahead of the camera | motion LOD's full-detail radius |

`check.motion` is the recommended motion LOD for that speed. `band_changes` (a dry run of `tune_fade_bands`) lists the
`fade_start` each short class would need (`fade_end` kept, never below a fifth of it): copy them into the
`.scatter.ron`.

## `terrain.set_forest_flight`

Any of:

- `motion`: `true`, `false` or `{ lookahead_frames, full_detail_m, max_step_m }`. LODs are chosen for where each
  approaching instance will be `lookahead_frames` frames ahead along the eye's last step (8 = one cross-fade, so the
  switch completes before the instance arrives), instances whose bounding sphere is nearer than `full_detail_m` always
  draw LOD 0, and an eye step longer than `max_step_m` is a cut. `true` takes 8 frames, no full-detail radius and
  50 m; `terrain.forest_flight { speed_m_s }` recommends values (speed / 3 s, at least 10 m, for the radius).
- `sway`: `true`, `false` or `{ frequency_10m_hz, damping_ratio, max_tilt_rad, half_tilt_gust_m_s, min_height_m,
  max_instances }`. Trees near the camera's wake lean as damped oscillators (default 0.6 Hz for a 10 m tree, damping
  0.1, at most 0.12 rad) and ring down. The whole instance leans about its base; motion vectors follow.
- `wake`: `{ body_radius_m, min_speed_m_s, strength, memory_s, max_gust_m_s }` of the built-in wake (an
  axisymmetric far wake along the last `memory_s` seconds of the camera's track; nothing below `min_speed_m_s`).
- `fps`: the rate the wake and sway step at (set the output rate for offline renders).
- `meter_pops`, `pop_threshold_px`, `reset_pops`: the pop meter on the extracted scatter draws. A pop is a whole draw
  that changes LOD, appears or vanishes while its centre is on screen and its projected radius is at least the
  threshold.

→ `{ before, after }`.

## A flythrough, step by step

1. `terrain.forest_corridor { points, times, carve: true }` on the planned path: read `violating_segments`, the
   `nearest` obstacles and `canopy_cover` (dip into the low-cover segments for the canopy bursts).
2. Move the path or `terrain.forest_carve` it; check `after.obstacle_violations == 0`.
3. `terrain.forest_flight { speed_m_s }` at the top speed: fix `fade_band_short` classes in the `.scatter.ron`.
4. `terrain.set_forest_flight { motion: <check.motion>, sway: true, fps: 60, meter_pops: true }`, render, then read
   `stats.pops_total`.

## Rust API

`regia_terrain::scatter::forest`: `ScatterQuery` (`in_box`, `along_segment`, `cell_hits`), `corridor_report`,
`carve_corridor` (`CorridorCarve::apply`), `canopy_gap_map` (power-law canopy gaps as a density map), `MotionLod`
(`ScatterField::set_motion_lod`), `flight_check`, `TrackWake` / `GustField` / `SwayField` (`step`, `apply_frame`,
`step_extracted`, `apply_extracted`), `PopMeter`, and the stress kit `synthetic_forest` / `standin_tree` /
`run_flythrough`. A host without the ECS frame calls `SwayField::step` and `apply_frame` on the draws `collect`
returns, each frame, with `1 / fps`.
