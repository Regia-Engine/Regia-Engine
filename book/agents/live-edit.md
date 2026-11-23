# Live edit: a running game in the Outliner and Inspector, and the live link

The Outliner and the Inspector can show more than the edit world (`docs/specs/19-editor.md` § 4,
ADR-7833). A **source switch** at the top of both panels lists:

| Source id | What it shows |
|---|---|
| `edit` | The edit world (the level being authored). |
| `play` | The in-process play world, while play runs (the panels' default during play). |
| `launch` | A running standalone launch (`editor.launch`). The launch gives the game its own agent directory, `<project>/.regia/launch/agent-<id>`, in `REGIA_AGENT_DIR`; a game built with the agent feature advertises its endpoint there. |
| `pie-server`, `pie-client-<k>` | The children of a separate-process PIE session (their agent directories). |
| `attached-<n>` | A runtime attached by a host through `regia_editor::remote_world::attach` (an agent directory). |

A runtime is reached through its **own agent server**, with the same client `regia agent call` uses:
the endpoint and token are read from its agent directory and only a loopback endpoint is dialled. The
editor reads it with the runtime methods `scene.query_entities` and `scene.get_entity` and writes it
with `scene.apply_edits`; the runtime's permission profile applies (a game whose `.regia/agent.toml`
withholds `edit_scene` refuses the writes, and the panel shows the refusal).

With a runtime selected, a banner says the changes are not saved: the runtime's world is not the level
on disk.

- **Remote Outliner.** A tree: one row per entity (its name and kind) under its parent, from the
  `parent` each `scene.query_entities` row names (a child whose parent is not on the pages read shows
  at the root). 200 rows a page; **More** reads the next page (at most 50 pages). The tree is read
  again at most every 500 ms while a panel shows it. The rows are keyboard rows: Tab reaches them, Up
  and Down move, Enter or Space selects, Ctrl+Enter (or Ctrl+click) adds to the selection, Left hides
  a row's children (or, on a row without open children, moves to its parent), Right shows them.
  **Show or hide children** does the same for the selected row. Which rows are collapsed is kept per
  source for the editor session (switching to another source and back keeps it).
- **Remote Inspector.** The selected entities' components as `scene.get_entity` gives them. Numbers
  are drag fields (or type a value and press Enter), switches are checkboxes, text is a text field
  (an asset path is text too: type the project-relative path), and a field whose type the editor knows
  as an enum is a choice of its variants without data (`Mode: Fixed`; a variant carrying data shows its
  fields). An edit writes the whole component, with the field changed, on **every selected entity
  that has the component**, in one `scene.apply_edits` transaction, and reads them back. **One
  gesture is one transaction**: a number dragged over many frames, or text typed into a field, is
  shown at once and sent when the drag ends or the field is left (Enter or Tab).

Remote calls never run on the editor thread. Each connection has its own thread that dials the
runtime and runs the calls in order, fed by a queue of at most 8 jobs; the panels queue a read or an
edit and draw what came back, so a slow or stopped runtime never stalls the editor. A full queue
drops a read (the next frame asks again) and keeps an edit for the next frame. The agent methods
queue their call and wait for its answer (at most 30 s, the jobs before it included). Each call has a
2 s read timeout; a connection that fails shows its error on the panel and is retried at the next
read.

## Live link

**Live link** (off by default) mirrors edits made in the edit world into the running session, one
way, by **entity guid**: the runtime selected in the source switch, else the play world while play
runs. Turn it on with the **Live link** toggle beside the source switch (shown while a session runs)
or `editor.live_link { on: true }`. It starts from the edit world as it is then; later edits are
mirrored at most every 50 ms (a drag sends at most 20 updates a second, under a runtime's default
limit of 50 mutating calls a second):

- a changed component of a level entity (a property set in the Inspector, a gizmo drag, an undo) is
  written whole to the session's entity with the same guid;
- an entity created in the edit world is spawned in the session with its components and its guid,
  under the session's entity with its parent's guid (a parent created in the same pass is spawned
  first, and the child finds it through the mapping that spawn recorded); a parent the session lacks
  is listed as a conflict and the child is spawned at the root. One the session already has (an
  undone delete) gets its components written instead;
- an entity deleted in the edit world is despawned there with its children.

Only level entities take part: entities with an `EntityGuid` that are not editor scaffolding
(`EditorOnly`). Engine-kept components (`GlobalTransform`, visibility, hierarchy relationships,
`EntityGuid`, `LevelMember`) and editor-only types are never sent. Nothing flows back: a change the
game makes stays until the same component changes again in the edit world.

An edit whose entity the session lacks (the game despawned it, or it never had it), or one the session
refuses, is not made and is listed as a **conflict** (the panel shows "N not mirrored", the list is in
the tooltip and in `editor.live_link`). When the session ends (play stops, the launch ends) the link
turns off.

Into a runtime, a pass runs on the connection's thread: one pass is in flight at a time, and edits
made meanwhile go with the next pass (the change ticks keep them). `editor.live_link` waits for the
pass in flight before it answers, so `pushed` and `conflicts` are current.

## `editor.remote_sources`

Read-only. No params. Returns `{ current, sources: [{ id, label, kind, remote, agent_dir }] }`:
`current` is the source the panels show now; `kind` is `edit`, `play`, `launch`, `device`,
`pie_process` or `attached`. `device` (id `device`, label "Device game <id> (<serial>)") is the game an
Android launch runs on a device, listed while its dev transport is open (ADR-8472): the editor reaches
it through the host port `adb forward` gives the game's agent port, with the one-time token the launch
handed the game in its intent. Selecting, inspecting, editing and the live link work as for `launch`.

## `editor.remote_select`

Read-only (it changes only what the panels show). Params: `source` (a source id; default keep the
current one), `entities` (runtime entity ids to select; a runtime source only), `add` (with
`entities`: toggle them in the selection), `pages` (pages of 200 rows to read, default 1, at most
50), `collapse` / `expand` (entity ids whose children the remote Outliner hides or shows again; kept
per source, the agent route of **Show or hide children**). Returns the remote view; `entities` lists
every row read in tree order (a collapsed row's children included) with its `parent` and `depth`,
`collapsed` the current source's collapsed rows:

```json
{ "source": { "id": "launch", "label": "Launched game 3", "kind": "launch", "remote": true,
              "agent_dir": "/home/me/game/.regia/launch/agent-3" },
  "connected": true, "error": null, "pages": 1,
  "entities": [{ "entity": 4294967301, "name": "Crate", "kind": null, "parent": null, "depth": 0 },
               { "entity": 4294967302, "name": "Lid", "kind": null, "parent": 4294967301, "depth": 1 }],
  "collapsed": [],
  "selection": [4294967301],
  "inspected": [{ "entity": 4294967301, "components": { "Transform": { "translation": [1.0, 0.0, 0.0], "...": "..." } } }] }
```

## `editor.remote_set`

Mutating, permission `play`. Params: `component` (the short name `editor.remote_select` shows, such
as `Transform`), `path` (the field inside the component's value: `translation/1`, `translation.y`,
`color[2]`; default the whole value), `value`. Sets it on every selected runtime entity that has the
component and reads them back. Returns `{ applied, entities, values: [{ entity, value }] }`.

## `editor.live_link`

Mutating, permission `play`. Params: `on` (turn the link on or off; omitted: answer the status).
Returns `{ on, session, target, pushed, conflicts: [{ guid, name, op, component, reason }] }`:
`session` is where edits would go now (`play` or a source id), `target` where the last pass wrote,
`pushed` the edits made so far, `op` one of `set`, `spawn`, `despawn`.

## Error codes

| Code | When |
|---|---|
| `remote_no_source` | The named source is not listed by `editor.remote_sources`. |
| `remote_not_connected` | The runtime could not be reached: no endpoint in its agent directory yet, the connection was refused or dropped, or a call timed out. |
| `remote_call_failed` | The runtime refused a call (its own error is in the message). |
| `remote_no_selection` | `editor.remote_set` (or `editor.remote_select { entities }`) without a runtime selected, with nothing selected there, or with no selected entity having the component. |
| `remote_bad_path` | `path` names no field of the component's value. |
| `live_link_no_session` | `editor.live_link { on: true }` with no play session and no runtime selected. |

## Known limits

- A device game is listed only for the editor's own Android launch of a dev build with the agent
  feature (`editor.launch { target: "android" }`, ADR-8472); a game started some other way is attached
  with `regia install-android --attach` and `attach`.
- An enum field is offered as a choice only when the editor itself registers the type (engine types
  and the project's game types the editor loaded); otherwise its variant name is edited as text.
  Switching to a variant that carries data is done with `editor.remote_set` and the whole value.
- An asset-path field is a plain text field in the remote Inspector (no picker: the runtime's world
  does not carry the field's asset filter).
- The live link mirrors the parent of a spawned entity; moving an existing entity under another
  parent in the edit world is not mirrored (the hierarchy relationship is engine-kept).

## Testing it

`crates/regia_editor/src/remote_world.rs` starts a headless runtime for its tests: a scene world
served by a real loopback agent server on its own thread, advertised in a unique agent directory, the
way a launched game is. The tests list it with `editor.remote_sources`, select it, check that the
remote Outliner draws its entities as accessible rows, set a field with `editor.remote_set` and read
the value back through the protocol and from the runtime's own world, and reach a remote row from
the keyboard. `crates/regia_editor/src/live_link.rs` starts a real play session and checks that a
gizmo drag (`SetTransform`) in the edit world moves the play entity with the same guid, that edits
reach a launched runtime by guid, and the spawn, delete and conflict cases.
