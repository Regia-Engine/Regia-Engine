# The Bake panel

The **Bake** tab (bottom dock, next to Kit) bakes indirect light for a part of the level into a `.fgiv` volume. You
choose a box, a quality and press **Bake**. This is the same bake as `regia bake-gi`
([Baking GI from meshes](../agents/baked-gi-bake.md)); the panel writes the description for you.

## Place the volume

- **Center** and **Size** are the world-space box, in metres. The green wireframe in the viewport is that box; the
  darker lines on its floor are the probes. The gizmo shows while the viewport's Bounds overlay is on, and
  **Show the volume in the viewport** switches it off.
- **Probe spacing** is the distance between probes. Smaller spacing means more probes and a longer bake. The line
  under the fields shows the probe count; it turns red when the volume is refused (a non-positive size or one over
  100,000 m on an axis, or more than 4,194,304 probes). Spacing runs from 0.05 m to 64 m.
- **Fit to selection** wraps the box around the selected entities with a 2 m margin.

## Choose a quality

**Draft** (64 rays, 2 samples, 3 bounces) is a quick look. **Normal** (128, 8, 6) is a working bake. **High** (256,
16, 8) is a final bake. **Advanced** shows the raw counts, the seed and the filter, and an option to name the volume
in the level when the bake succeeds.

## Bake

- **Dry run** checks the description and the layout. It needs no GPU and writes no volume.
- **Bake** runs the baker on the first GPU adapter. The bar shows the stage and the probes done. **Cancel** stops it
  at once; nothing is written.

The bake traces the level's static meshes as occluders. Mark a mesh entity `GiMobility::Static` (Inspector) to
include it; dynamic and built-in meshes are skipped. The sun is the level's first directional light.

The panel writes `bake/<level>.editor.bake.ron` in the project (outside `assets/`, so it is not shipped) and the
volume to `assets/maps/<level>.fgiv`. When the bake is done the panel shows the file. To use it, name it in the level
settings (`look.baked_gi`) or tick **Name the volume in the level when done** and save the level
([Baked GI from the level](../agents/baked-gi-maps.md)).

## Limits

- The editor runs the `regia` program next to it, else `regia` on the `PATH` (`cargo build -p regia_cli`); without
  it the panel reports `bake_spawn_failed`.
- One bake runs at a time. Closing the editor stops it.
- The volume is a single box. For automatic bricks over a whole level, interiors or terrain, write a `.bake.ron` by
  hand.
- The box has number fields but no drag handles in the viewport yet.
- An agent can do all of this: [Baking from the editor](../agents/baked-gi-editor.md).
