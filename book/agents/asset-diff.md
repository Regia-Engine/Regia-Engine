# Asset and graph diff against a revision (agents)

The editor compares an asset with an older revision of itself from git: material, animation and logic
graphs node by node and wire by wire, any other `.ron` asset field by field. Design: spec addendum § 10.8
(P-182), ADR-7799. Everything here is read-only: nothing writes the asset, the index or the repository.

## Methods

These are editor methods (registered by `regia_editor::asset_diff`). Paths are project-relative
(`assets/materials/hull.matgraph.ron`); an absolute path must lie inside the project.

| Method | Permission | What it does |
|---|---|---|
| `editor.asset_diff { path, rev_a?, rev_b? }` | read | Diffs `path` between revision `rev_a` (default `HEAD`) and `rev_b` (default: the working copy on disk). Answers the change list below. |
| `editor.asset_revisions { path, limit? }` | read | The file's history, newest first, following renames (`git log --follow`), at most `limit` entries (default 50, at most 200). What **Diff against revision…** lists. |

A revision is a commit hash, a branch or tag name, `HEAD`, `HEAD~2`, `main^` and the like. Ranges (`a..b`),
`<rev>:<path>` forms and anything that starts with `-` are refused before git runs. git runs with the project
root as its working directory, through an argument list (never a shell), so the project may sit in a
subdirectory of the work tree. When the file was renamed after `rev_a`, the old side is read under the name it
had in that commit (`path_a` in the answer).

### `editor.asset_diff` answer

```json
{
  "path": "assets/m.matgraph.ron", "kind": "material_graph",
  "rev_a": "HEAD", "rev_b": null, "hash_a": "1a2b…", "hash_b": null,
  "path_a": "assets/m.matgraph.ron", "path_b": "assets/m.matgraph.ron",
  "counts": { "added": 1, "removed": 1, "changed": 1 },
  "changes": [
    { "target": "node", "status": "changed", "id": "base", "old_id": "base", "kind": "Float",
      "fields": [ { "path": "0", "status": "changed", "old": "0.5", "new": "0.8" } ] },
    { "target": "node", "status": "added", "id": "gloss", "old_id": null, "kind": "Float", "fields": [] },
    { "target": "node", "status": "removed", "id": "spare", "old_id": "spare", "kind": "Float", "fields": [] }
  ]
}
```

`kind` is `material_graph` (`.matgraph.ron`), `material_function` (`.matfunc.ron`), `anim_graph`
(`.animgraph.ron`), `logic_graph` (`.logic.ron`) or `ron` (any other `.ron`). Each change is one of:

- `{ target: "node", status, id, old_id, kind, fields }`: a graph node that was added, removed or changed.
  `fields` lists the paths inside the node's value that changed. A node whose `id` differs from its `old_id`
  was renamed in place (matched by kind and canvas position, below).
- `{ target: "wire", status, from, to, slot }`: a wire that was added or removed. Material graph surface
  outputs are wires into `output` (`slot` is the output, such as `base_color`).
- `{ target: "field", status, path, old, new }`: a changed document path outside the nodes (parameters,
  shading, a rule list's settings), or, for a `ron` asset, any changed path.

`status` is `added`, `removed` or `changed`; values are compact RON text (cut at 200 characters).

### How graphs are compared

| Format | Nodes | Node id | Wires |
|---|---|---|---|
| Material graph, material function | the `nodes` map | the node name | each node's inputs, plus the surface or function outputs into `output` |
| Animation graph | every node of the tree, state machine states included | the node path (`root`, `root/b`, `root/state:Idle`) | each child into its parent's input slot |
| Logic graph | the rules | the rule name | none |

Nodes match by id first. A node left over on both sides then matches one of the same kind at the same
authored canvas position (within 1 unit, from the `.layout.ron` sidecar at the same revision): a node renamed
in place is one changed node, not a removed and an added one. A matched node is changed when its value
(without its canvas position) or its id differs; moving a node on the canvas alone is not a change. Wires
compare by their endpoints after the old ids are mapped onto their matches.

Paths: struct fields joined with `.`, list items and map keys in `[..]` (`nodes["albedo"]`, `rules[2]`), tuple
items as `.0`. Numbers compare by value (`10` equals `10.0`), struct fields and map entries regardless of their
order, and comments and layout do not count. Lists are aligned on their longest run of equal items, so an
inserted item is one `added` path, not a change of every item after it.

## Errors

| Reason | When |
|---|---|
| `no_project` | No project is open. |
| `bad_path` | `path` is missing or empty. |
| `path_outside_project` | `path` leads outside the project (`..`, an absolute path elsewhere, a symlink out). |
| `no_revision_control` | The project is not inside a git work tree, or `git` cannot be run. The window shows "No revision control". |
| `bad_revision` | A revision is malformed (a range, an option, a path part) or names no commit; or `rev_a`/`rev_b` is not a string. |
| `not_in_revision` | The file is not in that revision (under its current name or the name the history gives it there). |
| `read_failed` | The working copy cannot be read (deleted or unreadable). |
| `unsupported_asset` | The file is not a `.ron` asset. |
| `parse_failed` | A side is not valid RON, or not a valid graph of its kind. |
| `git_failed` | `git log` failed for another reason. |

`editor.asset_revisions` answers `{ path, revisions: [{ hash, short, date, author, subject, path }] }` (`path`:
the file's project-relative path in that commit) and refuses with the same reasons.

## In the editor

- Content Browser: a `.ron` asset's menu has **Diff against HEAD…** (`editor.asset_diff { path }`) and **Diff
  against revision…** (the history list of `editor.asset_revisions`; Enter or a click on an entry runs
  `editor.asset_diff { path, rev_a: <hash> }`).
- The material graph and animation graph panels' file rows have the same two buttons for the open graph's saved
  file (greyed out until it is saved).
- The diff window shows graphs side by side, old on the left and new on the right: added nodes in the theme's
  `ok` accent, removed ones in its `error` color, changed ones in its `warn` color, unchanged ones dim. Both
  canvases share one pan and zoom (drag or scroll either). The change list beside them is a keyboard list:
  Up/Down move, Enter or a click centers the change in both canvases. A canvas takes the keyboard focus with Tab;
  then the arrows pan and `+`/`-` zoom; **Zoom in**, **Zoom out** and **Fit** are buttons. Other assets show as a
  two-column tree (field, old, new): the changed paths, or every field with **Show unchanged fields**. **Pick
  another revision…** goes back to the history list. Nothing in the window edits the asset.

## Example

```json
{"method":"editor.asset_revisions","params":{"path":"assets/materials/hull.matgraph.ron","limit":5}}
{"method":"editor.asset_diff","params":{"path":"assets/materials/hull.matgraph.ron","rev_a":"HEAD~3","rev_b":"HEAD"}}
```
