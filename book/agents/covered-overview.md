# Covered overview

`covered_at` (spec 121 § 5.3, the `stage_b` feature) says whether a roof covers a point. The
overview and the minimap use that answer to hide covered ground and to keep tunnel mouths visible.
Per-unit view flags say which units the overview draws as rock-hidden or lit. The bake and the
flags live in `regia_world_geom::overview` (ADR-8333). That module does not call `covered_at`
itself: the caller passes the cover test, so the CLI and `terrain.unit_view` stay free of
`stage_b`.

## Bake

`regia terrain overview-bake <map> [--out DIR] [--json] [--size N] [--contour-step M] [--force]`
reads `heightfield.json` and `layout.json` from `<map>`, and `tunnels.json` when it is there.
It writes `overview_<id>.png` and `minimap_<id>.png`. `<id>` is the directory name. The default
output directory is `<map>`. `--size` defaults to 1024 and must be a multiple of 4, from 2 through
2048. `--contour-step` defaults to 5 metres; 0 turns contours off. Without `--force`, an existing
pair of PNGs is left in place (`skipped` in the JSON).

North (+Z) is up and east (+X) is right. The picture is a bilinear resample, a hillshade, a
water, ice or lava tint, contour lines, marker rings (portals, geysers, scrap, spawns, bases) and
a 3-pixel grey frame. The minimap is that picture divided by 4 on each side.

A bad map prints `overview_bake_input` and exits 2. A missing `tunnels.json` is not an error: the
bake has no portals. A missing heightfield, a missing layout, or a surface the bake does not know
(`cheese` is one) is an input error.

The CLI passes no cover predicate, so it does not hide covered ground. A host that has loaded a
cooked mesh calls `bake_directory` with a predicate, usually `covered_at` at about 1 m. 
`hide_covered_ground` paints every covered pixel `[8, 8, 10]` and then redraws each portal mouth
in white, so an entrance stays visible on a hidden roof. The same pixels are hidden in the
minimap, because the hide runs before the downsample.

`--json` prints `overview`, `minimap`, `side`, `mini_side`, `skipped`, `overview_blake3` and
`minimap_blake3`.

## View flags

`terrain.unit_view` is a read. It does not move anything and it does not query a mesh. The caller
says which units are underground.

Parameters:

- `team` (integer, required). The viewer's team. Team 0 is never radar-hidden.
- `viewer` (array of three numbers, required). Position `[x, y, z]` in metres. Used when
  `controlled` is absent or that id is not in `units`.
- `viewer_underground` (boolean, optional, default false). The viewer is under a roof.
- `controlled` (integer, optional). That unit is not radar-hidden. When it is listed in `units`,
  its position is the viewer.
- `units` (array, required, at most 4096). Each object has `id`, `team` and `pos` `[x, y, z]`,
  plus optional booleans `underground`, `entrance`, `lit`, `headlights` and `ping` (each defaults
  to false).

A call that omits `team`, `viewer` or `units`, or that sends the wrong shape, answers
`unit_view_bad` and does not panic. `{}` is that error.

The result is `{ units: [{ id, flags, shown }] }`, one entry per input unit, in the same order.
`shown` follows the satellite rule already in `compat::sense::satellite_shows`: an entrance is
shown, and any other underground unit is not. A unit that is not underground is shown.

`flags` keeps bits outside this mask and rewrites these four:

| Bit | Value | Meaning |
|---|---|---|
| `ROCK_HIDDEN` | `0x100` | Rock hides this enemy from radar. |
| `LIT` | `0x200` | The unit counts as lit. |
| `HEADLIGHT_ON` | `0x400` | Lamps are on, and the unit is underground. |
| `PING_FLASH` | `0x800` | A sweep is live, and the unit is underground. |

Radar hide applies only to an enemy (a team other than 0 and other than `team`, and not
`controlled`). It is set when exactly one of the viewer and the unit is underground and the
distance is past 150 m, or past 300 m when that unit is `lit`. `HEADLIGHT_ON` and `PING_FLASH`
follow `headlights` and `ping` on every team, including allies, when `underground` is set.

## Tests

`unit_in_tube_hidden_entrance_shown_on_overview` puts one unit under a roof and one at an
entrance. The tube unit is hidden and not shown; the entrance is shown. The same fixture clamps a
body under the lid, so the origin stays in the gallery. `covered_ground_is_hidden_in_the_minimap_bake`
checks that every pixel over a covered roof is the hidden color and every mouth pixel is white.
`minimap_bake_twice_identical_bytes` and the ported minimap cases live in `tests/minimap_bake.rs`.
None of these pin a query hash.
