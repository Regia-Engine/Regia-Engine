# Split baked GI volumes and the brick-loop probe

A baked GI volume (`.fgiv`) is uploaded into storage bindings, and the portable size of one binding is 128 MiB
(`regia_asset::baked_gi::PORTABLE_VOLUME_BYTES`, the mobile minimum-spec profile's limit). A map whose probe layout
needs more is baked as a **split volume** (ADR-2618): up to four part files, each a complete volume the renderer
blends with its neighbors. Maps that fit are baked exactly as before, as one file.

## Baking

`regia bake-gi` (and `--from-scene --bake`) does it on its own:

- `--dry-run` prints `plan.volume_bytes` (the whole layout) and `plan.split_parts`, the bytes of each part (empty for
  one volume).
- The bake writes part 0 where the volume would go (`--out`, or the level's `look.baked_gi.volume`) and part `k`
  beside it as `<stem>.part<k>.fgiv` (`maps/bore.fgiv`, `maps/bore.part1.fgiv`, ...). The summary's `parts` lists the
  files (`null` for one volume). Rebaking a map as fewer parts removes the stale part files.
- `bake_volume_too_large` is still the error for a layout no split fits: a single detail brick above 128 MiB, or a
  map that needs more than four parts. Its message says the largest part the best split had.
- Parts 1.. are written before part 0, each through a temporary file and a rename, so a reader that sees the new
  part 0 finds the new parts beside it.
- `--check-volume` and `--sample` take part 0 and read every part: the set is joined back into the volume the bake
  split (`BakedGiVolume::join_split_set`, byte for byte), so a split bake checks fresh against its description and a
  point in any part samples its baked light. A missing part or a part of another bake is `bake_volume_unreadable`
  (`--check-volume`) or `volume_unreadable` (`--sample`): these tools answer for the whole bake, unlike the runtime
  below, which lights what it has.
- `--dry-run` and `--check-volume` also print `memory`, the answer to "will this fit?" before a bake runs ([below](#memory-before-a-bake)).

### How a layout is cut

- The parts tile the map in X and Z (`tiles`, at most four parts). Cuts run between two terrain columns (a layout
  without a terrain layer uses its largest brick spacing as the column grid), near the size quantiles, and each cut
  moves to the nearest column no detail brick reaches across, so parts meet along brick boundaries. Where no free
  column is near enough, a brick goes whole into both parts that read it. Only when no such split fits is a brick
  **cropped** at the cut ([below](#cropped-bricks-adr-3237)).
- Every terrain column and every brick has exactly one **owner** part. The split section records the owned columns
  and each brick's owner, so a tool can count each probe once.
- Neighbors share a **band** one terrain cell wide, and each part holds one more guard column past it. In the band
  both parts read the same probes, copied byte for byte from the unsplit bake (same encode exposure, same filtered
  SH), so the light does not change across the cut.

## Loading

`BakedGiVolume::read_split_set(path)` reads part 0 and every part beside it, checking that they belong to the same
bake (`split.group`), count and index. The renderer takes the parts as layers: the level's own volume
(`set_baked_gi`) is part 0 and the rest go to `set_baked_gi_layers` at priority 0. Across each band the part's weight
ramps from 1 to 0 while the neighbor's ramps up, so the two sum to 1. That is the volume blend of
[baked-gi-seams](baked-gi-seams.md), but per edge: an edge on the map's own border does not fade, and a part whose
neighbor is not installed lights out to its own edge.

A level whose `look.baked_gi.volume` names part 0 of a split volume installs every part this way on its own
(`regia_framework::baked_gi_level`): each part is resolved like part 0 (the cooked asset, or the file under
`assets/`), and `level.get_baked_gi` `probes` counts every part that is lit. Naming a part other than 0 is
`volume_unreadable`. A streamed level's split volume joins the blend with every part at its fade weight, after the
map's own parts. The level reloads when part 0 or any part file changes, appears or goes away.

### A missing or damaged part (ADR-3006)

A part that does not load leaves the others lit. It may be missing (`volume_not_found`), damaged, of another version
or of another bake (`volume_unreadable`), and part 0 is no different: without it the lowest part that loads becomes the
renderer's volume. The other parts light out to their own edge on the side of the lost neighbor, and the ground only
the lost part held gets the frame's other GI (Balanced or Depth). `error` and `error_code` report the first failure,
`parts.failed` in `level.get_baked_gi` (and `failed_parts` per tile in `level.get_baked_gi_stream`) lists each one.
Nothing installs only when no part at all loads.

A **reload never loses lit parts**: when a read of the same path finds fewer parts than the frame already has (a
rebake in progress, a file being replaced), the lit parts stay and the error is reported. The set is read again as
soon as a part file changes, so it completes on its own.

### Host layers (ADR-3006)

A host's own layers (`set_baked_gi_layers`) and a level's parts share the renderer's list. The framework adds its layers
ahead of the host's and removes only its own, so the host's stay while a split or streamed level is open and after it
closes. A host may replace its layers at any time; the framework's come back on the next update. A layer the host holds
can be read back with `baked_gi_layers` and passed to `set_baked_gi_layers` again, parts included, without doubling
them. `level.get_baked_gi` `host_layers` counts the layers that are neither the level's nor a streamed level's. The
frame blends at most 8 volumes, in priority order, and within a priority the framework's come first, so a host's layer
is what drops when the list is past the cap (`baked.volumes_dropped`).

The screen pass reads volumes from four storage slots. Volumes are packed first fit, one binding per slot, so a
device whose binding holds one part (the 128 MiB minimum spec) installs four parts. A device with large bindings
keeps everything in slot 0, as before.

## Memory before a bake

`regia bake-gi --dry-run` and `--check-volume` print `memory` for the layout the description would bake, and
`level.get_baked_gi` prints it for the volumes in the renderer (the volume and every layer, each its own buffer; `null`
when there are none). No GPU, no probes: it is arithmetic over the layout.

```json
"memory": {
  "bytes": 438000000, "parts": [109000000, 110000000, 110000000, 109000000], "binding_limit": 134217728,
  "contributors": [{ "target": "terrain", "bytes": 437000000 }],
  "tiers": [
    { "tier": "minimum", "vram_budget": 6979321856, "budget": 872415232, "bytes": 438000000, "budget_used": 0.5,
      "fits": true, "over": [], "fixes": [], "fixes_suffice": true },
    { "tier": "deck", "vram_budget": 3221225472, "budget": 402653184, "bytes": 438000000, "budget_used": 1.09,
      "fits": false,
      "over": [{ "code": "baked_gi_over_budget", "target": "volume", "bytes": 438000000, "limit": 402653184 }],
      "fixes": [{ "fix": "spacing", "target": "terrain", "action": "double terrain_spacing (8 m to 16 m)",
                  "bytes_after": 110000000 }],
      "fixes_suffice": true }
  ]
}
```

(The values are illustrative.)

- `tiers` is one entry per `regia_render::BudgetTier`. `budget` is baked GI's share of the tier's VRAM budget, an eighth
  of it (`baked_gi_budget`: 832 MiB minimum, 384 MiB Deck, 1.25 GiB desktop high, 192 MiB integrated low). The host holds the same bytes again
  in RAM while the level is open.
- `over` names what is past a limit. `baked_gi_over_budget`: the whole set against the tier's share.
  `baked_gi_part_over_binding`: one volume or part past 128 MiB, which does not install on every device.
- `fixes` lists what brings it in, the largest saving first, and `bytes_after` is the total after that fix and the ones
  before it (an estimate). `spacing` names the terrain layer or a brick by the plan's label (`interior:shaft`,
  `cluster:hangar`, ...). A `split` fix means a rebake writes parts: it fixes the binding only, since a split never
  shrinks a volume. `fixes_suffice: false` means the list ran out first, so coarsen across the layout. A layout past
  every split fails the description first with `bake_volume_too_large`, so `--dry-run` reports that error instead.

Tall maps split in X and Z too, and ADR-3006 has the numbers. A brick past one binding no longer stops a split: it is
cropped (below). What still stops a map is its total, since four parts of one binding hold 512 MiB at most, less the
bands and crop margins.

### Cropped bricks (ADR-3237)

When no split that keeps every brick whole fits (one tall brick past 128 MiB, a stack of bricks on one spot), the bake
crops the bricks that cross a cut (format version 5). Each part holds the brick's cells over its own rectangle plus a
**margin** of whole cells past each cut side, and owns the cells on its side of the cut, so every probe still has exactly
one owner and every record is a byte copy of the unsplit bake's. A cropped split cuts on the finest brick cell instead of
the terrain's columns, and its band is one such cell wide: a cut through a tall brick then costs a few of its cells, not
a terrain cell (often 8 m) of them. A layout that split before ADR-3237 splits into the same parts, with nothing cropped.

The margin keeps the light continuous across the cut. The lookup picks the brick a point lies deepest in, and a crop
face can only make a point look shallower, so the margin is at least the deepest point of any overlapping brick of the
same class (interior or not) plus a cell, and never under two cells. Inside each part's rectangle the pick, the
coverage and the interpolation cell are the unsplit ones (`a_cropped_split_blends_to_the_unsplit_lookup_on_the_cpu`,
`a_frame_across_a_cropped_cut_equals_the_unsplit_frame`). That holds for `normal_bias + view_bias` up to one cell (the
defaults are 0.4).

Where to see it:

- `--dry-run` and the bake summary: `plan.split_cropped_bricks`, the cropped bricks per part (empty when nothing is
  cropped), beside `plan.split_parts`.
- `--check-volume`: `split`, one row per part: `{ "bytes", "bricks", "cropped_bricks" }` (`null` for one volume).
- `level.get_baked_gi`: `parts.installed`, one row per lit part: `{ "index", "bytes", "bricks", "cropped_bricks" }`.
- `bake_sizing plan --level tall`: `split_study.split_parts` and `split_study.split_cropped_bricks`.

`regia upgrade-project --write` rewrites version 4 part files at version 5 (no brick cropped); version 4 files also
load as they are. An unsplit volume is the same bytes in both versions apart from the version word.

## Agent surface

`render.get_gi_mode` `baked` gains:

- `volumes`: one object per volume the last frame blended, in blend order: `probes`, `bytes`, `slot`, `priority`,
  `weight` and `split` (`null`, or `{ index, count, group, joined }`, where `joined` lists the edges (`-x`, `-z`,
  `+x`, `+z`) across which the part blends with an installed neighbor).
- `volume_slots`: storage slots in use (at most 4).
- `brick_visits`: the brick-loop probe's last histogram (below), or `null`.

A part that does not join its neighbor (an empty `joined` where you expect one) means that neighbor is not
installed: check `volumes_dropped` and `error`.

## The brick-loop probe

`render.set_baked_gi_brick_probe {"on": true}` makes the screen pass count, for every lit texel, how many detail
bricks its lookups tested (the length of the brick grid cell's candidate list, summed over the blended volumes;
ADR-1835). The counts go into a 64-bin histogram that is read back without a stall, about three frames later, into
`render.get_gi_mode` `baked.brick_visits`:

```json
{ "histogram": [5120, 1800, 240, 0, ...], "texels": 7160, "mean": 0.32, "max": 2, "captures": 12, "bins": 64 }
```

Bin `k` counts the texels that tested `k` bricks, and the last bin holds every count of 63 or more. `max` is the
highest bin with a texel. The light does not change while the probe runs. A held view (camera, sun and draw list
unchanged) is served by the static-screen cache and records no lookup, so nothing is counted until the view moves:
step the game or move the camera between the calls.

To time the brick loop on a card or a phone, take the screen pass's GPU time with the probe off: `profiler.frame`
GPU pass `gi probes` (`GpuTimings::gi_ms`, which on a baked frame is the baked screen lookup). Then read `brick_visits` once with it on, so the time comes with the
bricks per texel it paid for. Switch the probe off again before you measure (its atomics cost a little).

Errors: `render.set_baked_gi_brick_probe` without a boolean `on` is `F0003` (invalid params). In an app without a
runtime renderer it is a not-found error that names `regia_render::RenderPlugin`.
