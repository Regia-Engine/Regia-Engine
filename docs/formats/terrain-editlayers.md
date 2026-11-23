# `<map>.terrain.editlayers` (terrain edit layers, version 2)

Owner: `regia_terrain::edit_layers` (`TerrainEditLayers`, ADR-7825, ADR-8381). The saved form of a terrain's non-destructive edit
layers: the base heights where a layer covers them, each layer's values, per-layer material-weight channels, and an optional
named procgen target. Next to the map and named like its holes file
(`maps/arena.level.ron` → `maps/arena.terrain.editlayers`, `edit_layers_path`); a level that names its terrain puts it under
that map's name. Written by every level save after a layer change or a layered stroke; read when the editor opens the level
(`regia_editor::terrain_layers::load_level_edit_layers`). The runtime never reads it: the cooked heightfield (`.fckd`) holds
the flattened result, so the cooked format does not change.

Version 2 is the v0.6 fold of version 1 (ADR-8381). A version 1 file loads through `migrate_edit_layers_bytes`: every height
value keeps its `f32` bits, weight channels are absent, and no layer has a procgen target. `regia upgrade-project` runs that
migrator. A version 2 blob migrates to itself, byte for byte.

| Field (postcard order) | Meaning |
|---|---|
| `version` | u32 varint, 2. Read before the body: any other version is refused (`edit_layer_file_version`), except version 1, which the loader migrates |
| `width`, `height` | u32 varints, the heightfield's grid; `2..`, at most 8193 × 8193 samples, read and checked before any array; must equal the terrain's grid (`edit_layer_size_mismatch`) |
| `chunk` | u32 varint, 64: the edge of the stored chunks |
| `active` | `Option<u32>`: the layer strokes write (`None`: the base) |
| `next_id` | u32 varint: the next layer id (ids are never reused) |
| `base` | chunks of base heights (meters), only where some layer covers the map |
| `layers` | the layers, bottom first |
| `base_weights` | chunks of base material weights (version 2). A missing sample is channel 0 at weight 1 |

A height chunk is `{ x, y, values }`: chunk column and row and the row-major `f32` values of the `64 × 64` block at
`(x · 64, y · 64)`, clipped at the map edge. A layer is
`{ id, name, enabled, locked, opacity, blend, values, coverage, weights, procgen }`:
`id` > 0 and unique, `name` 1..=64 printable characters, `opacity` in `0..=1`, `blend` `Add` or `Set`, `values` chunks (Add:
delta in meters; Set: target height in meters) only where the layer has an effect, `coverage` chunks (Set only, each in
`0..=1`; empty for Add). `weights` is a list of weight chunks. `procgen` is `None` or `{ graph, output }`: `graph` is 1..=256
characters and `output` is 1..=64, neither empty after trim and neither containing a control character (`edit_layer_bad_target`).
Every value must be finite. At most 32 layers.

A weight chunk is `{ x, y, channels }`. `channels` holds `samples × 8` finite numbers, sample-major, eight channels per sample
(the terrain material stack). A chunk is stored only when some sample in it is not the default. Untouched samples inside a
stored chunk are filled with the default and dropped again on load. The base default is channel 0 = 1 and the rest 0. A
layer's default is all zeros (a missing sample is a zero delta). An explicit all-zero base paint is stored and is not the
same as a missing sample.

**The height fold.** The terrain is the base folded through every enabled layer, bottom first. A run of consecutive Add layers adds
`Σ opacity · delta`, summed in `f64` in layer-id order and rounded once, so reordering Add layers among themselves changes no
bit. A Set layer moves the height below it toward its target by `coverage · opacity` (at 1 it is the target exactly), so
moving a Set layer past other layers changes the result. Weights do not enter the height fold.

**The weight fold** (ADR-8202, accepted by ADR-8381). At each sample, start from the base weights. Each enabled layer with
opacity above 0 adds `opacity ×` its signed delta, per channel. Clamp every negative channel to 0, then divide by the channel
sum once. A sum of 0 falls back to channel 0 at weight 1 (the base material). The folded channels sum to 1.

**Procgen target.** `procgen` names the graph and output that own the layer. Publish from that output writes only that layer.
If two layers name the same target, the topmost (last in the list) wins. A brush on an owned layer is refused
(`edit_layer_owned`) until the target is detached. Detach is an undo step. Cleanup removes values from that layer only.

**Reading against the map.** Samples outside every stored height chunk take the base from the cooked map (no layer covers them, so
the map is the base there). After decoding, the fold over every stored chunk and every layer's bounds must reproduce the
cooked map either as the exact authored f32 height or its exact 16-bit cook-then-decode height
(ADR-8202); otherwise the file is stale (the terrain changed outside its layers) and is refused
(`edit_layer_file_stale`). A refused file is left on disk untouched and the editor shows no layers. Weight-only and
procgen-only edits do not change heights, so they do not fail this check.

**Versioning.** Registered in `cargo xtask check-formats` (`terrain-editlayers`) and `regia upgrade-project` (reads the leading
postcard version and migrates version 1). A further layout change bumps `version` and ships a migrator.
