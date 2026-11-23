# Baked GI from the level

A map can name its own baked GI volume. The framework loads the `.fgiv` when the level opens and
installs it in the renderer, so a host no longer calls `set_baked_gi` itself. The volume and its
lookup are in [Baked GI volumes](baked-gi.md); this page is about how a level carries one.

## The level names its bake

`LevelSettings.look.baked_gi` (scene format v10, `docs/formats/scene-files.md`):

```ron
look: (
    baked_gi: Some((
        volume: "maps/bore_twelve.fgiv",      // under assets/
        settings: (sky_scale: Some(0.8)),     // optional per-map overrides
    )),
),
```

- `volume` is a path under `assets/`. `assets://` and `assets/` prefixes are accepted. An empty
  path names no bake.
- `settings` overrides the renderer's baked-GI settings for this map. Each field is optional, and
  an unset one keeps the host's value: `intensity`, `sky_scale`, `emissive_scale`,
  `replace_live_fill`. Biases and sun/fill following stay with the host.
- Levels older than v10 migrate with `baked_gi: None` and light as before.

## What the framework does

`regia_framework::baked_gi_level::BakedGiLevelPlugin` is part of `DefaultPlugins`.

- **Level loads:** reads the volume (the cooked asset from a pak or the dev cache; in a dev run
  without a cook, the file `assets/<path>`), installs it, and applies `settings` on top of the
  settings the host had.
- **Hot reload:** a recooked `.fgiv`, a changed file, or an edited path or override installs the
  new volume without reopening the level. A file that appears later (a bake finishing) is picked up
  too.
- **Level unloads:** the volume is removed and the host's settings come back. Only the volume the
  level installed is removed. A volume the host installed itself stays, and a level that names a
  volume replaces it.
- **Failure:** a missing file or one that does not load logs one warning and installs nothing. The
  frame keeps its other GI (Balanced or Depth). Nothing panics. A split volume with a part lost lights
  the others ([Split baked GI volumes](baked-gi-split.md#a-missing-or-damaged-part-adr-3006)).

## Entities: baked or dynamic

`GiMobility` is a component with two values: `Static` (the bake reads this entity) and `Dynamic`
(the default: lit from the volume at runtime, not part of it). An entity without it is `Dynamic`,
so existing scenes are unchanged. It is separate from the `Static` marker used for batching. The
export that builds a bake description from a level's `Static` entities is a follow-up
(ADR-284); until then the description is written by hand (`regia bake-gi`).

## Agent method

`level.get_baked_gi` (read only) returns:

| Field | Meaning |
|---|---|
| `plugin` | the framework plugin is installed |
| `level` | the level's `baked_gi` (`volume`, `settings`), or `null` |
| `volume` | the path the plugin is working on, or `null` |
| `installed_by_level` | the level's volume is the one in the renderer |
| `source` | `level`, `host` (the host installed another volume) or `none` |
| `error`, `error_code` | why it could not load, and a stable code |
| `probes` | probes in the level's installed volume |
| `renderer` | `{ installed, active, probes, gpu_bytes, error }` from the renderer, or `null` |
| `parts` | the split set: `{ count, lit, failed: [{ index, path, code, error }], installed: [{ index, bytes, bricks, cropped_bricks }] }`, `null` before a volume was read; a whole volume has `count` 1 (ADR-3006). `installed` lists each lit part, with the bricks it holds cropped at a cut ([cropped bricks](baked-gi-split.md#cropped-bricks-adr-3237), ADR-3237) |
| `host_layers` | layers in the renderer that are neither the level's parts nor a streamed level's (ADR-3006) |
| `memory` | the volumes' bytes against every tier, what is over and what fixes it ([details](baked-gi-split.md#memory-before-a-bake)), `null` with none installed (ADR-3006) |

Error codes: `volume_not_found` (no cooked asset and no file under `assets/`) and
`volume_unreadable`. That second code is a damaged or newer-version file, a split part whose group
does not match part 0, or a path that names a split part other than 0. Read `error`: when it says
`name part 0`, point `level` at the file without a `.part` suffix. Rebaking writes another good part
file and the same code comes back until the path is part 0.

## Checking a map

1. `level.get_baked_gi`: `source` is `level`, `error` is `null`, `probes` matches the bake.
2. `play.step 2`, then `render.get_gi_mode`: `baked.active` is `true`
   ([Baked GI volumes](baked-gi.md)).
3. If `error_code` is `volume_not_found`, check the path against `assets/` and run `regia cook`;
   if `volume_unreadable`, read `error`. A split part other than 0 needs the part-0 path; a damaged
   or newer-version file is rebaked with the same engine version.
4. `level.set_look {"look": {"baked_gi": null}}` removes the bake on the next frame, to compare.

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
level.get_baked_gi {}          # { plugin, level, volume, source, error, probes, renderer }
```

## Tunnels

A tube lights well from a ground-following layer along its floor: the sun that falls through the mouth
bounces down the tube (the tunnel acceptance scene's volume reads 440 lux/π on the floor 6 m in and 64 at
18 m). The volume lights single-sample frames only, so a tunnel view with MSAA shows no bake. Inside an
`InteriorVolume` that holds the camera the forward pass takes the interior shade, and since ADR-7975 that
shade reads the baked volume too: where the volume covers a surface inside the interior box, its baked
irradiance (and its probe light in the reflection) replaces the interior's neutral floor and the legacy
probe-grid bounce, so a baked tunnel keeps its light gradient on every desktop tier (Low included) instead
of a flat floor. Where no volume covers the surface (a tube without a bake, a stretch the bake missed),
the interior shade is the floor and probe bounce as before, bit for bit. Before ADR-7975 the interior shade
ignored the volume (ADR-3257), so a baked tunnel had to be lit without an `InteriorVolume`
(`probe_volume_lifts_the_tube_interior`, [Tunnel acceptance scene](tunnel-scene.md)). The Mobile tiers'
lite forward shader on a tile GPU reads no baked volume anywhere, inside or out (ADR-1874).
`cargo test -p regia_render --test interior_baked_gi` (lavapipe) holds the interior floor to the bake's
gradient on Desktop Low and High and the tube without a covering volume to the flat floor.
