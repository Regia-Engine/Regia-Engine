# Content Chunks

Panel id: `content_chunks`.

The **Content Chunks** tab shows how your project's cooked assets split into content chunks
(the base game, DLC and downloadable packs) under the `[cook.chunks.*]` rules in `Regia.toml`. It shows
what the next cook writes, using the same functions the cook uses, so you can check a rule change
before you cook, and ask "why is this asset in this chunk?". Depth: [Content Chunks panel
(agents)](../../agents/content-chunks-panel.md), [Content chunks](../../agents/content-chunks.md).

## Open it

The tab is in the default bottom group (**View ▸ Reset layout** restores a closed one). An agent opens it with
`editor.layout_apply {"open": "content_chunks"}`. It needs an open project that has been cooked: with no project it
shows `content_chunks_no_project`, and with no cooked registry for the platform `content_chunks_no_cook` (cook
first). The first time the tab is drawn it computes its view.

## Walk-through: find out why an asset is in a chunk

1. Open **Content Chunks**. The table lists one row per chunk in mount order with **Chunk**, **Delivery**, **Assets**,
   a size bar and **Requires**.
2. Choose what the bars show with **Disk** (the chunk's pak bytes; 0 until a cook has written the chunk paks) or
   **Memory** (the cooked bytes of its members). The **Platform** box picks the cooked cache; empty means this
   machine's platform. Press **Refresh** after a cook or after you edit the rules.
3. Click a chunk's name. Below the table it lists **Direct assets of `<chunk>`** (the ones its own rule selected), each
   with the selector that picked it, and a collapsible **Size map of `<chunk>`**: its assets, largest first, each with a
   bar relative to the largest and its type. Both lists show at most 100 entries.
4. Click an asset path in either list, or type an `assets/...` path (or an asset id) into **Why is this asset here?** and
   press **Explain** (or Enter). The answer reads `<path> is in chunk <chunk> (<reason>)`, then the rule selector, the chain
   back to a direct asset (`via a <- b`), and the other chunks holding a copy.
5. Under it, **References** lists what the explained asset references and **Referenced by** what references it; an entry in
   another chunk names that chunk. Click any path to walk the graph.
6. Reference problems are listed in yellow under the table; errors among them fail the next cook.

You can also right-click an asset in **Content** and choose **Why is this in its chunk?**: the toast names the chunk and
reason, and this tab holds the details.

The chunk rules themselves (include globs, tags, requires, delivery, priority, **Add chunk**, **Remove rule**) are edited in
the **Project Settings** tab ([Project settings](project-settings.md)); press **Refresh** here afterwards.

## What is on screen

| Action | Control / key | Agent method |
|---|---|---|
| Read the chunks and their sizes | the table | `editor.content_chunks_state` |
| Switch the bars between disk and memory | **Disk**, **Memory** | `editor.content_chunks_state` with `show` |
| Pick the platform's cooked cache | **Platform** box | `editor.content_chunks_state` with `platform` |
| Recompute | **Refresh** | `editor.content_chunks_state` with `refresh: true` |
| Select a chunk | Click its name | `editor.content_chunks_state` with `chunk` |
| Size map of a chunk | **Size map of `<chunk>`** | `editor.content_chunks_size_map` |
| Explain an asset | **Explain**, or a path link | `editor.content_chunks_why` |
| References of an asset | **References** list | `editor.content_chunks_references` |
| Change a chunk rule | Project Settings tab | `editor.content_chunks_set_rule` |
| Page and filter the unshipped assets | **Unshipped**: **Filter**, **Previous**, **Next** | `editor.content_chunks_unshipped` |
| Preview a settings change before saving | Project Settings: **Preview chunk assignment** | `editor.content_chunks_preview` |

## Keyboard

Enter in the **Why is this asset here?** box runs **Explain**. Nothing else has a shortcut; the buttons and links are
focusable (Tab, then Space or Enter).

## Known limits

- It reads the cooked asset registry, so it needs a cook first; **Refresh** recomputes the view from the current
  `Regia.toml` rules and that registry.
- **Disk** bars read 0 for a chunk until a cook has written its pak.
- `Platform` must be empty or a short lowercase tag (letters, digits, `_`, `-`); anything else is refused.
- Direct assets, size-map rows, references and referrers each show at most 100 entries. An agent reads more:
  `direct_assets_total` in `editor.content_chunks_state`, `limit` on `editor.content_chunks_size_map`, and the full
  lists of `editor.content_chunks_references`.

## See also

[Project settings](project-settings.md), [Content browser](content-browser.md), [Size Map](size-map.md), [Content Chunks
panel (agents)](../../agents/content-chunks-panel.md), [Content chunks](../../agents/content-chunks.md).

## Missing dependencies stop shipping

A shipped asset's required dependency must have a registry record, a cooked file and a place in
the shipped content. This applies to both chunks and named build profiles, even with cook
validation set to `Warn` or `Off`. `content_dependency_missing` tells you whether the target ID
is unknown, the profile excluded a required target, or its cooked file is missing. The message
and JSON `root_path` show how the shipped root reaches it. Restore the target, fix its reference,
or remove the exclusion, then cook and package again.

`content_registry_invalid` means the registry is unreadable or malformed. Rebuild the cooked
cache; the tools will not replace a corrupt registry with an empty dependency graph. An absent
registry still permits the legacy cache-manifest fallback. `regia assets chunks --json` exposes
`complete` and structured `problems`, so tools can distinguish a diagnostic report from a
complete shipping graph. Reference cycles are allowed and do not count as missing assets.

For a deliberate optional exclusion, keep using the referring asset's `optional_references`
tag with a matching target path glob. This only covers a known target; it cannot make an
unknown ID optional. Field-level optional-reference annotations are not supported. The existing
pak hash checks and unwritten-asset diagnostics remain in place.

## Why each root ships, and dynamic roots

The panel, `regia cook` and `regia package --profile` find their starting points the same way: the
startup level (or a profile's entry levels), every top-level `ui/*.ui.ron`, the procedural bakes of
each level reached, and what those files name. The panel's state lists each root with the reason it
exists, for example `declared_by: "[cook.chunks.base] entry[0]"` or `via: "levels/a.level.ron"`.

A level or texture your game opens by a name it builds at run time (a level-select screen, a random
skin) is named by no file, so nothing finds it. Declare it as a **dynamic root**: add its path or a
glob to the chunk's `entry` list, and to the profile's `include` list. Both then ship it and say
why.

If a file's format has no typed reader (a JSON table, a custom text file), its strings are still
followed, but the report marks it under root coverage as a fallback scan: check that what it names
is what you expect. A level or UI layout that does not parse, or a file that cannot be read, stops
the cook and the package with a `content_root_*` code instead of shipping less.

## Choosing what ships: closure settings

By default every cooked asset ships (`content = "all"`), and an asset no rule selects goes to `base`. With
**closure** content only what the game can reach ships: the startup level, the top-level UI layouts,
what they reference, and the **entry roots** you declare per chunk for content the game loads by a name
it builds at run time. Everything else is listed as **Unshipped**. Both are set in **Project Settings**
under `[cook.chunk_options]` (**Content: all / closure**) and in each chunk rule (**Entry roots**,
comma-separated globs under `assets/`).

### Walk-through: leave old content out on purpose

1. Cook the project, then open **Project Settings**. Set **Content** to **closure**.
2. Press **Preview chunk assignment**. A moment later (it computes on a worker thread) it shows how many
   assets ship and how many do not, each declared root, and the assets whose chunk changes:
   `assets/ui/old_logo.png: base -> unshipped` is the old logo nothing uses any more. That is what you
   want, so leave it.
3. Press **Save**, then **Refresh** in **Content Chunks**. **Unshipped** lists the logo; type `ui/` (or a glob
   such as `ui/*.png`) into **Filter** to find it among many, and use **Next** to page through long lists:
   each page holds 100 paths and the header gives the total. Click a path to see what references it.
4. Changed your mind? **Edit ▸ Undo** puts `Regia.toml` back exactly as it was, comments included.

### Walk-through: a dynamic root that is missing

1. Your wave spawner loads `spawn/wave_01.table` by name. In the preview it is in the **unshipped**
   count, because no file names it.
2. In the `base` rule, type `spawn/**` into **Entry roots** and preview again: it moves from
   `unshipped` to `base`, and the root reads `[cook.chunks.base] entry[0] = spawn/**`.
3. Mistype it as `spwan/**` and preview: the root is red (**matches nothing**) and the preview says
   `will not save: content_chunks_entry_unmatched`. **Save** refuses with the same message and writes
   nothing, so the project never claims a working closure that would ship without the tables. Fix the
   glob and save.

If you edit `Regia.toml` or re-cook while a preview is computing, the preview reads **stale** and is
dropped; press the button again. Without a cook, roots cannot be checked: the save goes through and the
toast says the entry roots were not checked, and the next cook checks them.
