# Asset browser: search, thumbnails, large projects (`editor.asset_thumbnails`)

The Content Browser is built to stay smooth with tens of thousands of assets (ADR-1329). It works on
top of the [content index](editor-content-index.md):

- **A folder view costs the folder.** The index keeps each folder's own assets, so opening a folder of
  100 assets in a project of 50 000 touches 100 entries, not 50 000.
- **Typing narrows.** A search that only narrows the previous one (one more letter, one more word, an
  added filter) filters the previous hits instead of every asset. `search_tested` and
  `searches_narrowed` on `editor.content_index` count this.
- **Outside changes re-list one folder.** The once-a-second folder-time check, `refresh: true`, and the
  live asset watcher re-list only the folders that changed. New folders are walked and removed ones
  forgotten. `dirs_read` and `refreshes` count this; `scans` (full walks) stays at 1.
- **The grid draws only the tiles on screen**, and asks the thumbnail cache only for those.

## Search syntax

The search box, `editor.content_index`'s `search` and `editor.list_assets`'s `search` all take the
same query: words separated by spaces, matched without regard to case.

| Word | Meaning |
|---|---|
| `rock` | The project-relative path contains `rock` (any case). Several words must all match. |
| `props/rock` | Words may hold `/`; Windows paths match too. |
| `-lod` | Hide paths that contain `lod`. |
| `type:mesh` or `t:mesh` | Only one asset type: `level`, `prefab`, `mesh`, `texture`, `material`, `audio`, `other`. Several `type:` words widen the filter. |
| `ext:png` | Only file names ending in `.png` (`ext:material.ron` works too). |

Results are ranked: first the file names that start with the first word, then the file names that hold
every word, then the paths that match only through a folder name. Each rank is sorted by path.

A blank search lists the selected folder. A search that is not a valid query lists nothing; the browser
shows the reason under the toolbar, and `editor.content_index` fails with `data.index_error` set to the
code:

| Code | Cause |
|---|---|
| `search_bad_type` | `type:` names an unknown asset type. |
| `search_empty_filter` | `type:`, `t:`, `ext:` or `-` has nothing after it. |

```json
{ "method": "editor.content_index", "params": { "search": "rock type:mesh -lod", "limit": 20 } }
```

`editor.content_index` gains `limit` (paths returned, default 200, at most 5000) and the counters
`dirs_read`, `refreshes`, `search_tested` and `searches_narrowed`.

## Thumbnails

The browser's **Grid** toggle (beside **List** in the toolbar) shows tiles with pictures. Textures
(`png`, `jpg`, `jpeg`, `tga`, `bmp`, `hdr`, `exr`), glTF models (`glb`, `gltf`) and materials
(`.material.ron`, `.matgraph.ron`, `.matinst.ron`, drawn on a sphere) get pictures; other files show
their icon.

- **Memory.** The last 512 pictures (96 px) stay in memory; the least recently drawn are dropped first.
- **Disk.** Pictures are stored in `<project>/.regia/thumbs/<key>.png`. The key hashes the cache
  version, the picture size, the path, and the source's length and modification time. An edited file
  therefore gets a new picture, and an old picture is never shown as current. When the folder holds
  more than 256 MiB, the oldest pictures are removed until it holds three quarters of that. The folder
  is a cache: deleting it is always safe.
- **Off the frame.** A background thread reads stored pictures and makes texture pictures. Model and
  material pictures are rendered on the GPU, one per frame. A tile that scrolls out of view before
  its request is handled drops the request.
- **Changes.** The live asset watcher tells the cache which files changed. With the watcher off
  (`REGIA_ASSET_WATCH=0`), an edited texture keeps its old picture until the editor restarts.

## `editor.asset_thumbnails`

Read-only. Requests pictures for up to 256 project-relative paths and reports each one's state.

Params:

- `paths` (required): 1 to 256 paths under `assets/`, such as `assets/textures/rock.png`.
- `wait` (default true): finish the requests before replying. Models and materials render only when
  the editor has a GPU device. With `false`, the requests are only queued.
- `clear_memory` (default false): drop the in-memory pictures first, so the next answers come from
  disk.

```json
{ "method": "editor.asset_thumbnails", "params": { "paths": ["assets/textures/rock.png", "assets/props/crate.glb"] } }
```

Returns:

- `thumbnails[]`: one `{ path, state, origin?, error? }` per path. `state` is one of:
  - `ready`: the picture is in memory; `origin` says how it got there (`disk`, `made` or `rendered`);
  - `pending`: queued (only with `wait: false`);
  - `needs_gpu`: a model or material waiting for a GPU render (a headless editor has none);
  - `failed`: unreadable, undecodable, or nothing to draw; `error` says which;
  - `unsupported`: no picture for this kind of file.
- `stats`: `px`, `memory_entries`, `capacity`, `queued`, `in_flight`, `gpu_waiting`, and the counters
  `requests`, `memory_hits`, `file_stats`, `dispatched`, `disk_hits`, `made`, `rendered`, `failed`,
  `evicted`, `dropped`, `stale_results`, `pruned_files`.
- `cache_dir`: the disk cache folder.

The counters are how an agent checks the cache without a clock. A second call for the same paths adds
`memory_hits` and no `made`. With `clear_memory: true`, it adds `disk_hits`.

Errors (`data.thumb_error`):

| Code | Cause |
|---|---|
| `thumb_no_project` | No project is open (a `not_found` error). |
| `thumb_bad_params` | `paths` is missing, empty, longer than 256, or holds a non-string. |
| `thumb_bad_path` | A path is absolute, contains `..`, or is not under `assets/`. |

## `editor.content_view`

Switches the browser between the list and the thumbnail grid, for example before a panel capture.
Params (both optional; without them the call only reports): `grid` (true for tiles, false for the
list) and `tile` (tile edge in points, 48 to 192, default 96). Returns `{ grid, tile }`. The panel
uses the new view from its next frame.

```json
{ "method": "editor.content_view", "params": { "grid": true, "tile": 128 } }
```

A wrong type or a tile outside 48..192 is an `invalid_params` error with
`data.view_error = "view_bad_params"`, and nothing changes.

## Checking it

- `editor.content_index { search: "prop_1" }` then `{ search: "prop_12" }`: `search_tested` grows by the
  first search's `visible`, not by `assets`, and `searches_narrowed` grows by one.
- Copy a file into a folder, then `editor.content_index { refresh: true }`: `stale` is true, `dirs_read`
  grows by one (the folder), and `scans` is unchanged.
- `editor.asset_thumbnails` twice on the same textures: the first reply has `origin: "made"` (or
  `"disk"` when the cache already had them), and the second adds only `memory_hits`.
