# Baked lighting panels: Bake, Bake runner and Lightmaps

Panel ids: `bake`, `bake_runner` and `lightmaps`.

Three tabs run the offline lighting bakes from the editor. Each bake runs as a child process
(the `regia` command-line program next to the editor, else `regia` on the `PATH`, or the one named by
the `REGIA_CLI` environment variable), so the editor keeps drawing, **Cancel** stops it at once, and
a crash in a bake cannot take the editor down. One bake runs at a time per tab.

| Tab | Runs | Use it to |
|---|---|---|
| **Bake** | `regia bake-gi` on a description the tab writes | Place one probe volume in the level, choose a quality and bake it |
| **Bake runner** | `regia bake-gi` on a built-in scene or a project `.bake.ron` | Run a hand-written description with progress and cancel |
| **Lightmaps** | `regia bake-lightmaps` | Bake per-mesh lightmaps for the open level or a description, and review the atlas |

The Bake tab is described step by step in [The Bake panel](../bake-panel.md). Depth:
[Baking from the editor](../../agents/baked-gi-editor.md), [Bake runner](../../agents/editor-bake.md),
[Lightmaps](../../agents/lightmaps.md).

## Open them

The three tabs are in the default layout's bottom group; **View ▸ Reset layout** brings back a
closed one. An agent brings one forward with `editor.layout_apply` and the id `bake`, `bake_runner`
or `lightmaps`. There is no palette command for these tabs.

## Run the Bake runner

1. Open **Bake runner**. The **Scene** drop-down lists the built-in scenes (`cornell`, `furnace`,
   `rolling-hills`) and every `.bake.ron` in the project; **Refresh** rescans the project.
2. Leave **Output** empty for the default, or type a `.fgiv` path.
3. Optionally open **Overrides** to set rays per probe, samples, bounces, seed or probes per batch,
   or to skip the visibility filter.
4. **Validate** is a dry run: it checks the description and shows the layout without a GPU. **Bake**
   traces. The bar shows the stage and the probes done; **Cancel** stops it and writes nothing.
5. When it ends the tab shows a summary (probes, bricks, size, device, time) or the failure code,
   and a **Log** section with the bake's last lines.

## Run the Lightmaps tab

1. Open **Lightmaps**. **Source** is the open level by default; the drop-down also lists the
   project's levels and descriptions and the built-in scenes.
2. Set **Texels per metre**, **Samples per texel**, **Bounces**, **Largest atlas side** and
   **Atlas pages at most**, and the checkboxes (**Bake the direct sun**, **Skip the filter**, **Planar
   charts**, **On the GPU** (needs a CLI built with the `lightmap-gpu` feature), **Visibility
   clusters**, **Denoise**, **Crease placement**, **Stitch seams**, **Auto density**) and **Encoding**.
3. **Lay out** is a dry run that lays the atlas out and previews density without baking. **Bake**
   bakes; **Cancel** kills it and leaves the old output.
4. The result shows the atlas size and entry count. **Density** colors each entry by its texel
   density against the density asked for; **Seams** colors by chart-seam error against the
   **Budget**. **Seams in the viewport** draws the worst seams in the scene. **Entries** lists each
   mesh with its texels per meter and seam numbers.
5. A bake that was interrupted may leave a checkpoint: the tab shows its status with **Discard**,
   which deletes it so the next bake starts over.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| Read, set, start and cancel the Bake tab's volume bake | **Bake**, **Dry run**, **Cancel** | `editor.bake_get`, `editor.bake_set`, `editor.bake_start`, `editor.bake_cancel` |
| Start a Bake runner bake | **Validate**, **Bake** | `editor.bake_gi` |
| Read the runner's progress and summary | The state area | `editor.bake_status` |
| Cancel the runner's bake | **Cancel** | `editor.bake_gi_cancel` |
| Start a lightmap bake or dry run | **Bake**, **Lay out** | `editor.lightmap_bake` |
| Read its progress and the atlas view | The state area and preview | `editor.lightmap_status` |
| Cancel it | **Cancel** | `editor.lightmap_cancel` |
| Density or seam preview, seam budget, seams in the viewport | **Density**, **Seams**, **Budget**, **Seams in the viewport** | `editor.lightmap_preview` |

## Keyboard

None of the three tabs handles keys of its own; reach their controls with Tab and press them with
Enter or Space.

## Known limits

- The CLI program must be found; without it a bake reports `bake_spawn_failed`.
- The Bake tab's volume is one box; for bricks over a whole level, write a `.bake.ron` and use the
  Bake runner.
- The seam **Budget** here is a preview value; a shipping package reads `[lightmaps] seam_budget` in
  `Regia.toml`.

See also: [Look-dev](lookdev.md), [Editor guide](index.md).
