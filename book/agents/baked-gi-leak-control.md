# Baked GI leak control

A baked GI volume (`.fgiv`, [Baked GI volumes](baked-gi.md)) stores light at probes on a grid, and the
runtime interpolates between them. Where a wall stands between two probes, the light of one side must not
reach the other. This page covers what the bake and the lookup do about it (ADR-366, on top of ADR-206,
ADR-279 and ADR-281) and how an agent checks it.

## What keeps light where it belongs

- **Probes inside geometry** are moved out by relocation or marked invalid (`INSIDE_GEOMETRY`), and probes
  under the terrain are invalid (`UNDER_TERRAIN`). The lookup skips invalid probes.
- **Walls between probes.** After the trace, `regia bake-gi` tests the straight segment between every pair of
  neighboring probes against the map's meshes and terrain. Where a segment crosses geometry:
  - the 3×3×3 smoothing filter does not average the two probes;
  - the probe at the low end stores where the wall is on the segment (a 4-bit code per axis in its flags).
  At runtime, a lookup cell whose corners carry such a code uses only the corners on the receiver's side of
  the wall. A doorway leaves some segments open, so the two sides stay joined there and light comes in through
  the door only.
- **Interiors.** A brick inside a DO27 interior that is not `sky_open` (the hangar, the hab) is written with the
  INTERIOR flag: inside it the terrain layer's outdoor light is skipped (ADR-281). The borehole shaft is
  `sky_open` and gets sky from above. An interior with `openings` is INTERIOR as a whole in v0.2.0.
- **Sealed rooms see no sky** in the trace itself: their rays hit the walls.

The distance moments of each probe still weight the corners as before. The wall test covers the case they miss:
a coarse moment map averages a thin wall with the open space beside it.

## Checking a bake

`regia bake-gi <map>.bake.ron` prints a summary whose `report` holds:

| Field | Meaning |
|---|---|
| `wall_pairs_tested` | Neighbor segments tested (0 when the wall test is off). |
| `wall_pairs_blocked` | Segments that cross geometry; the filter skips them. |
| `wall_probes` | Probes whose flags carry a wall code. |
| `interior_bricks` | Bricks written with the INTERIOR flag. |
| `interior_sky_probes` | Valid probes inside INTERIOR bricks that saw the sky (more than 1 % of their rays escaped). Probes on a brick's faces do not count: they may stand outside the walls. A sealed room reports 0; a room with an opening reports the probes that see out of it. A sealed room with a non-zero count has a hole in its walls. |

The bake reports a `walls` progress stage between the trace and the encoding.

Checks an agent can make after a bake:

1. `report.interior_bricks` equals the number of `plan.bricks` with `interior_flag: true`.
2. `report.wall_probes > 0` on any map with set pieces (a map of terrain only may report 0).
3. For each sealed interior, `report.interior_sky_probes == 0`. If not, look for a gap in the walls.

## Checking the installed volume

`render.get_gi_mode` returns `baked.wall_probes`: the installed volume's probes with a wall code. It is 0 for a
volume baked before the wall test (v0.1.27 and older bakes): those still load and look up exactly as before, but
without wall control. Rebake to get it.

Screenshots for a look check: one inside each interior near a wall, one outside along the same wall, with
`render.set_gi_mode {mode: "baked"}`, then with `{mode: "balanced"}` for comparison.

## Limits

- One wall position per segment: two partitions inside one cell read as one wall at their middle.
- The wall test sees triangles and the terrain. Glass blocks like a wall; alpha-tested foliage counts as solid.
- No light passes through an opening of an INTERIOR brick (the brick ignores the outdoor layer as a whole).
- A segment that runs to a probe under the floor or inside geometry crosses that surface, not a wall, so its code
  says little about where the wall is. The ADR-861 leak guard (`BakedGiSettings::leak_guard`, on by default)
  ignores such codes when it places the wall. `render.sample_baked_gi` shows the lookup at a point with the guard
  on and off. See [Baked GI quality](baked-gi-quality.md).
