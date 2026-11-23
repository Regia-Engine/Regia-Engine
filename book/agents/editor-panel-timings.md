# Editor panels at scale (`editor.panel_timings`, `editor.outliner_rows`)

Large projects (ADR-7306) put 100 000 entities in the Outliner, 100 000 lines in the Output Log and
lists of 100 000 elements in the Inspector. These panels lay out only what is on screen:

- **Outliner.** The hierarchy is flattened into the listed rows (the search, "Group by category" and
  every row's open/closed state applied). The tree is rebuilt only when the level changes (a spawn,
  a despawn, a reparent, a rename or a kind change), the rows only when the tree, the search, the
  grouping or an open/closed state changes, and only the rows in view are drawn. Shift-click ranges
  and the arrow keys walk every listed row, on screen or not. Up / Down move the selection (Shift
  extends it); Left / Right close and open the selected row. A selection made outside the panel (a
  viewport pick, `editor.select`) opens the entity's parents and scrolls its row into view.
- **Output Log.** The panel keeps its own history of the last 100 000 lines, polled from the log ring
  that `logs.read` reads (the ring itself keeps the last 4 096). A new line is tested against the
  filters once; changing a filter tests the history once. Rows are one line high; a click on a row's
  text shows its whole message, wrapped, under the list (Close hides it). Links in a row stay
  clickable. New lines keep the view at the bottom.
- **Inspector.** A list, array, map or set longer than 200 elements shows one page of 100, with
  first / previous / next / last buttons and a jump to an index. Edits on any page record the
  element's own path (`values[150]`), as `editor.set_property` does.

Agents read the same data without the panels: `editor.hierarchy` and `editor.outliner_rows` for the
tree, `logs.read` for log lines, `scene.get_entity` and `editor.set_property` for any element of a long list.

## `editor.panel_timings`

Read-only. The dock times every panel's draw and keeps the last 240 draw times per panel. A soak
script draws the editor for a while (scrolling, editing), then reads which panel's time grows.

Params: `panel` (only this panel, by its dock title such as `"Outliner"` or `"Output Log"`), `last`
(how many of the newest times to return per panel, 1 to 240, default 60), `clear` (forget every
recorded time after reading; default false).

```json
{ "method": "editor.panel_timings", "params": { "panel": "Outliner", "last": 30 } }
```

Returns `{ keep, preview_jobs, panels: [{ panel, draws, last_ms, median_ms, max_ms }] }`: `keep` is the ring size
(240), `draws` the draws recorded since start or the last clear, `last_ms` the newest draw times in
milliseconds, oldest first. A panel that was never drawn is not listed. `preview_jobs` is the number of
preview renders queued or running on the editor's preview thread (ADR-8584).

### Previews render off the UI thread

The Material Graph preview and its node swatches, the rig preview of the animation panels and the warm-up of
the viewport's camera preview render on a background thread, so the frame that opens a panel never waits for a
shader compile. The panel shows its last picture (or "Compiling the preview…", the line rig, an empty camera
inset) until the render ends, and the next frame shows it. A script that screenshots a preview waits for it:
`wait_until` on `editor.panel_timings` with `preview_jobs` equal to 0, then one more frame.

Errors: `F0002` when `last` is not an integer from 1 to 240 or `panel` is not a string
(`data.param` names it).

## `editor.outliner_rows`

Read-only for the level (it changes only the Outliner's view). Returns the Outliner's listed rows as the panel lists
them: the search and "Group by category" of its last draw, closed rows' children left out, a window at a time. It
works without a panel draw.

Params: `open` and `close` (arrays of entity ids whose rows to open or close), `reveal` (an entity id: open its
parents; the panel scrolls to its row on its next draw), `offset` (first row returned, default 0), `limit` (rows
returned, default 100, at most 1000), `open_levels` and `close_levels` (arrays of document ids, as
`editor.levels_open` lists them, whose level groups to open or close).

```json
{ "method": "editor.outliner_rows", "params": { "reveal": 4294967360, "limit": 20 } }
```

Returns `{ listed, filter, grouped, offset, rows, revealed, reveal_index, tree_builds, flattens }`: `listed` is the
number of listed rows; each row is `{ entity, label, depth, has_children, open }`, or `{ category, count, open }` for a
heading of the grouped view. While more than one document is open (a level opened beside the host, or a layer in its
own file) the hierarchy view lists each document's roots under its heading row `{ level, label, count, open, active }`
(`level` is the document id, `label` the file name with ` *` when unsaved and `(active)` for the active level; roots in
no open document come last under "Not in an open level", id `00000000-0000-0000-0000-000000000000`); with the host
alone there are no level rows. `revealed` says whether the revealed entity is listed and `reveal_index` is its row;
`tree_builds` and `flattens` count the tree rebuilds and row rebuilds so far (a soak checks they stay flat while
nothing changes).

Errors: `F0002` when `open`/`close` is not an array of entity ids, `open_levels`/`close_levels` is not an array of
document ids, `reveal` is not an entity id, or `offset`/`limit`
is out of range (`data.param` names it).

The other new controls are view state over data agents already reach: the Inspector pager (`scene.get_entity` reads
the whole component; `editor.set_property` takes any element path such as `samples[150]`) and the Output Log's
expanded row (`logs.read`).

## Verify

- Spawn a few thousand entities (`scene.spawn` in a loop), draw a few frames, then
  `editor.panel_timings { panel: "Outliner" }`: the times stay flat as the count grows.
- `cargo test -p regia_editor --test large_project -- --nocapture` prints each panel's median draw
  time (as a ratio to a 200-label control panel), allocations and text shapes per frame on a
  synthetic project (100 000 entities six levels deep, 100 000 log lines, a 100 000-element list,
  50 000 assets on disk), and asserts their bounds.
