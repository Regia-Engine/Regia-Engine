# Generate procedural content

<!-- index: Procedural graph -->
<!-- index: Biome -->
<!-- index: Procedural plant -->
<!-- index: Pgraph -->

A procedural graph (`.pgraph.ron`) places points, meshes, roads, and plants from a seed. This is
the path from a graph file to a bake the level loads without running the graph. The agent
reference is [Procedural generators](../../agents/procgen.md). Biomes are
[Biomes](../../agents/biomes.md). The specs are:

| Spec | What you use it for |
|---|---|
| `docs/specs/62-procedural-content-graph.md` | The graph, the run, the bake (`.pgout`) |
| `docs/specs/63-procedural-node-library.md` | The built-in node types |
| `docs/specs/64-splines-polygons-roads.md` | Splines, polygons, modules along a path, roads |
| `docs/specs/65-biomes-and-vegetation-scatter.md` | What grows in a kind of place |
| `docs/specs/66-tiled-and-runtime-generation.md` | Cells and tiers around an anchor |
| `docs/specs/67-procedural-graph-editor.md` | The Procedural Graph panel |
| `docs/specs/68-procedural-plants.md` | Trees and bushes grown from rules, exported as glTF |

The editor pages for the panels are [Procedural graph](../../user-guide/editor-guide/procgen.md)
(panel id `proc_graph`) and [Biome](../../user-guide/editor-guide/biome.md) (panel id `biome`).
This how-to does not describe the panel layout. Those pages do.

## 1. Check the graph

```sh
regia procgen graph check assets/procgen/forest.pgraph.ron --json
```

`--project` is the project root when the graph calls another graph or a variant parent.
The default is the current directory. The command exits 0 when `errors` is empty. Exit 1 means
the graph does not compile. The messages are the same list the editor shows. Fix those before
you bake. A graph that compiles can still place nothing if its bounds, its filters, or its
seed leave no points. That shows up in the run report, not in `graph check`.

In the editor, double-click the `.pgraph.ron` in the Content browser. The Procedural Graph tab
is the canvas: add a node, connect pins, set the seed, and preview. Edits are undoable with the
rest of the level. The same edits are `procgen.graph_edit` and the `editor.procgen_*` methods
on the panel page.

## 2. Bake the level, then verify the bytes

A generator in `Baked` mode (the default) stores its output as `<guid>.pgout` next to the level
and loads that file instead of running. From a shell, with no editor:

```sh
regia procgen bake levels/world.level.ron
regia procgen verify levels/world.level.ron
```

`bake` writes `<level>.procgen/` (`--out` picks another directory). `verify` re-bakes into a
temporary directory, compares bytes, and deletes that directory. It does not write the bake
directory. Exit 0 means every file matches. Exit 1 prints `changed:`, `missing:`, or `extra:`
(an `extra:` line tagged `orphan_bake` is a `.pgout` no current generator writes).

```sh
regia procgen bake levels/world.level.ron --prune --dry-run
regia procgen bake levels/world.level.ron --prune
```

`--dry-run` only prints what `--prune` would delete. `--prune` is refused for a single
generator, and `verify --prune` is refused. A file whose entity is still in the level is kept.

One generator, by guid or entity name:

```sh
regia procgen bake <guid-or-name> --level levels/world.level.ron
```

The command-line bake has no physics world and no terrain plugin. A graph that casts rays
against physics sees no hits, the same as `procgen.bake` in a world without physics. Bake in
the editor when the graph must see the terrain or the physics scene, then `verify` the bytes.

`regia upgrade-project` rewrites an older `.pgout`. A version 2 bake that should have held
generated meshes or terrain edits fails its hash (`pgout_corrupt`) and needs a new bake.

## 3. A biome

A biome (`.biome.ron`) says what grows in a kind of place: layers, entries, footprints, and
filters. **Window ▸ Biome** edits one file. **Content ▸ New ▸ Biome** creates an empty one.
The Area tool on the procedural viewport mode reads the project's `.biome.ron` files.

An older scatter set becomes one biome file per set:

```sh
regia procgen import-scatter assets/scatter/pines.scatter.ron --out assets/biomes
```

Paint or draw biome areas (a volume, a closed spline, or a biome-map texture) and bake as in
step 2. Two biomes that meet blend across a band. The definition rules and the refusal cases
are on the [Biome](../../user-guide/editor-guide/biome.md) page. Saving drops RON comments.
There is no autosave on that tab.

## 4. A road, a fence, or a plant

Splines, polygons, modules along a path, and road profiles are spec 64. The node catalog and
the road recipe are on [Procedural generators](../../agents/procgen.md). A road ribbon on the
terrain material, without a graph, is the terrain chapter's road decal. The graph is how a
path follows a spline and cuts junctions.

Plants are a separate graph domain (spec 68). They grow in the editor and on the command line.
A game ships the meshes, not the growth step.

```sh
regia procgen plant assets/procgen/oak.pgraph.ron --out assets/plants --variants 4 --seed 1
```

That grows four variants, seeds 1, 2, 3, and 4. Each export is `<name>_<k>.glb`, where `name`
is the graph's export stem and `k` runs from 0. The index is `<stem>.plants.ron`. When the
stem is `oak` and the graph has one export, the files are `oak_0.glb` through `oak_3.glb` and
`oak.plants.ron`. The glTF uses the tree conventions the scatter importer already reads. A
graph whose domain is World is refused (`procgen_wrong_domain`). A graph with no
`plant.export` node fails with `plant_no_export`.
`--param name=value` overrides one graph parameter (the value is RON). Repeat the flag for
several parameters.

## 5. Generate around the player

Spec 66 splits a generator into world-aligned cells and into tiers (a coarse pass, then a fine
one). Cells publish around anchors at run time, inside a per-frame budget, and match a bake of
those same cells. `procgen.bake` with `cells` rewrites only the named cells. This scheduler is
not the level's streaming grid. A streamed level still uses
[Stream a level](streaming.md) for entities. A tiled generator uses its own cells for points.

The editor's preview can follow the view. That control is on the Procedural Graph panel.

## Finished check

All of these are true:

1. `regia procgen graph check <file> --json` exits 0 and `errors` is empty.
2. `regia procgen verify <level>` exits 0. A mismatch names the file. Re-bake, then verify again.
3. For a plant, `regia procgen plant` exits 0, prints each `wrote` line, and the output directory
   contains one glTF per variant plus the `.plants.ron` index.
4. In the editor, preview the bound generator and take `seeing.screenshot` after
   `procgen.run` with `wait: true`. `procgen.list` reports `missing_meshes` empty for every mesh
   you expected to see. A path in that list has no entry in the mesh library, so it is not drawn.

Two runs with the same graph, the same seed, and the same inputs produce the same points.
Changing the seed is how you get another forest from the same graph.

## Views the panel page does not list

The Procedural Graph panel in this version includes a parameters tab, node badges, a biome
overlay, and a plant preview strip. The methods are `editor.procgen_param`,
`editor.procgen_badges`, `editor.procgen_biome_overlay`, and `editor.plant_preview`.
`docs/specs/67-procedural-graph-editor.md` describes that editor as built. The panel page's
last paragraph still says those views live on other branches. That sentence is out of date.
Use the methods here, and [Procedural generators](../../agents/procgen.md), for those views.
