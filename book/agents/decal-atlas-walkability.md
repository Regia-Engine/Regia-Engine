# Decal atlas and stair walkability

`nav.check_stair` asks whether a capsule can walk one stair. `vfx.damage_decal_atlas` describes a cook-side damage decal atlas and builds its pixels only when asked.

## `nav.check_stair`

Read-only. The call builds a small navmesh for that stair and does not read a `NavMesh` resource. An empty object fails with `nav_stair_input` before any mesh is built.

Parameters:

- `x` (number, required): world X of the bottom of the run, metres.
- `z` (number, required): world Z of the bottom of the run, metres.
- `direction` (string, required): `+x`, `-x`, `+z` or `-z`.
- `width_m` (number, required): width across the run, metres.
- `y0_m` (number, required): world Y of the lower landing, metres.
- `rise_m` (number, required): height gained over the run, metres.
- `run_m` (number, required): horizontal length of the run, metres.
- `step_rise_m` (number, optional): riser of flat treads, metres. Omit it for one ramp.
- `agent` (object, optional): capsule fields `radius`, `height`, `max_climb` and `max_slope` (radians). Omitted fields stay at 0.35 m, 1.6 m, 0.5 m and 45 degrees.
- `cell_size_m` (number, optional): horizontal voxel size, metres. Default 0.2.
- `cell_height_m` (number, optional): vertical span-merge gap, metres. Default 0.1.

A successful result is `{ "flagged": bool, "flag": "...", "connected": bool }`. `flag` is `walkable`, `too_narrow`, `too_steep`, `rise_exceeds_climb` or `not_connected`. `flagged` is true when `flag` is not `walkable`. `connected` is true when a path joins the two landings.

Error `nav_stair_input` (`F0002`): a parameter is missing or the wrong type, a number is not finite, or the stair or capsule is not usable (non-positive, an axis longer than 64 m, or more than 64 treads). The same code covers a probe the navmesh builder refuses.

```json
{ "x": 0, "z": 0, "direction": "+x", "width_m": 2, "y0_m": 0, "rise_m": 2, "run_m": 0.6 }
→ { "flagged": true, "flag": "too_steep", "connected": false }
```

## `vfx.damage_decal_atlas`

Read-only. The atlas is 1024 by 1024 RGBA8, sixteen cells of 256 pixels, for multiply-over-paint damage (holes, craters, scratches, soot, a weld, cracks, rivets). It is not the runtime 64 by 64 projected atlas.

`generate` (boolean, optional) defaults to false. An empty call returns the size and does not build pixels. `generate` true also returns `bytes` (the RGBA8 length) and `hash` (sixteen hex digits, FNV-1a 64 of those bytes). Two calls with the same inputs return the same bytes.

Error `decal_atlas_input` (`F0002`): `generate` is present and is not a boolean.

```json
{ "generate": false }
→ { "width": 1024, "height": 1024, "cells": 16, "cell_px": 256, "generated": false }
```
