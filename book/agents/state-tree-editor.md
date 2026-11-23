# State tree editor

State trees are authored as `.statetree.ron` files (states by name, child lists, transitions, see
`docs/formats/statetree-ron.md` and [state trees](state-tree.md)). The editor's **State Tree Editor** panel shows
one as a node view: states laid out by hierarchy with their parent links, transitions drawn as arrows, the
active path filled green while the game plays, states with errors outlined red. Next to it are a palette (a new
child state, a new transition from the selected state, the registered tasks), an inspector for the selected
state (name, parent, selection, weight, fallback, tasks, parameters, transitions, and the enter condition and
transition guards typed as RON, such as `Compare(key: "dist", op: Lt, value: 12.0)`) with its problems inline, the
full problem list, and the play view (agents, the watched tree's active path, what the last save did to the
running agents).

The `ai.statetree_*` methods check and edit files without the editor; the `editor.statetree_*` methods drive the
open panel. Both use the same operations, validator and canonical text (`regia_ai::state_tree_edit`, ADR-3342).
The file format did not change (version 2).

## Operations

`ai.statetree_edit` and `editor.statetree_edit` take a list of `ops`, applied in order, all or nothing: the first
op that does not fit the tree leaves it as it was and names its position and reason. Enum fields use the file's
spelling: `"trigger": "Tick"` or `{ "Event": "heard" }`, `"target": { "State": "Alert" }` or `"Parent"`,
`"selection": "Utility"`, `"condition": { "Compare": { "key": "dist", "op": "Lt", "value": 12.0 } }`,
`"value": { "Float": 1.5 }`.

| `op` | Fields | Does |
|---|---|---|
| `add_state` | `name`, `parent?`, `index?` | Adds a state. With a `parent` it is listed there (at `index`, default last). The first state of an empty tree becomes the root; any other state without a parent stays unreachable until it gets one. |
| `remove_state` | `name`, `subtree?` | Removes the state. Its children take its place in its parent's list, or go too with `subtree: true`. Transitions anywhere that target a removed state are removed. The root can only be removed when it has exactly one child (which becomes the root). |
| `rename_state` | `name`, `to` | Renames the state and every reference: the root, the parent's child list and every `State` transition target. The tree keeps compiling. |
| `reparent_state` | `name`, `parent`, `index?` | Moves the state under `parent` (or within the same parent: a reorder), at `index` of the list after it was taken out. `name` may be an included state (`Combat/Fight`), which lists it under `parent`. |
| `set_root` | `name` | Makes a state with no parent the root. |
| `set_state` | `name`, `selection?`, `enter?`, `weight?`, `utility?`, `clear_utility?`, `fallback?` | Changes the given fields. |
| `set_tasks` | `name`, `tasks` | Replaces the state's tasks (registered task paths; each task's own enter, tick and exit run with the state). |
| `set_ambient` | `tasks` | Replaces the tree's ambient tasks (ticked first on every tick). |
| `set_param` | `state`, `key`, `value?` | Sets one blackboard literal the state writes on entry (`enter_set`, the state's parameters); no `value` removes it. |
| `add_transition` | `state`, `trigger`, `condition?`, `target`, `priority?`, `hold?`, `index?` | Adds a transition (default last). |
| `remove_transition` | `state`, `index` | Removes a transition. |
| `set_transition` | `state`, `index`, `trigger?`, `condition?`, `target?`, `priority?`, `hold?` | Changes the given fields. |
| `move_transition` | `state`, `from`, `to` | Moves a transition in its state's list (declaration order breaks priority ties). |
| `add_include` | `prefix`, `path`, `index?` | Adds an include. Its states are named `prefix/<name>`; list the included root under a state with `reparent_state` to reach it. |
| `remove_include` | `prefix` | Removes the include and every child entry and target that names one of its states. |
| `set_include` | `prefix`, `to_prefix?`, `path?` | Changes the include; a new prefix is carried into every reference. |
| `set_tree` | `description?`, `seed?`, `state_key?` | Changes the tree fields. |

### Edit reasons

A refused op answers `F0002` with one of: `unknown_state`, `duplicate_name`, `empty_name`, `would_cycle` (the new
parent is the state or below it), `is_root` (the root cannot be moved), `cannot_remove_root`, `has_parent`
(`set_root` on a listed state), `bad_index`, `bad_value` (a non-finite number, an empty key or path),
`unknown_include`, `duplicate_prefix`, `too_many_states`.

## Files: `ai.statetree_validate`, `ai.statetree_edit`

Paths are project-relative, end in `.statetree.ron` and may not contain `..`.

```json
{ "method": "ai.statetree_edit", "params": { "path": "assets/ai/guard.statetree.ron", "create": true, "ops": [
    { "op": "add_state", "name": "Root" },
    { "op": "add_state", "name": "Idle", "parent": "Root" },
    { "op": "add_state", "name": "Alert", "parent": "Root" },
    { "op": "add_transition", "state": "Idle", "trigger": { "Event": "heard" }, "target": { "State": "Alert" } },
    { "op": "set_param", "state": "Alert", "key": "speed", "value": { "Float": 2.5 } } ] } }
```

`ai.statetree_edit` validates the result and saves the canonical text atomically (a temporary file, then a
rename) unless `save: false`. **A result with validation errors is refused** with `invalid_tree`, the issues in
`data.issues`, and the file is not written; `save: false` previews any result with its issues instead.
`create: true` starts an empty tree when the file does not exist. The answer is
`{ path, saved, valid, tasks_checked, issues, states, root, ron }`. `ai.statetree_validate { path | text }`
answers `{ path, ok, errors, warnings, tasks_checked, issues }` and never writes.

The canonical text has the tree fields, then one block per state with one field per line (defaults left out)
and one line per transition, so a diff of two saves shows what changed. Saving an unchanged document gives the
same bytes.

Task paths are checked against the registered tasks when any task is registered (`tasks_checked: true`); with
none registered (an editor with no game loaded) the check is skipped. Includes are read from the project.

File-method reasons: `missing_param`, `bad_path`, `not_found`, `parse_failed`, `bad_ops`, `too_many_ops` (more
than 512 ops), `invalid_tree`, `write_failed`, and the edit reasons above.

### Issue codes

Each issue is `{ code, severity, path, state, message }`. `path` points at the field: `root`, `states/Idle`,
`states/Idle/children/1`, `states/Idle/transitions/0/target`, `states/Idle/tasks/2`, `ambient/0`, `includes/1`
(a state with an empty name is `states/#<index>`). The validator lists every problem at once; the compiler stops
at the first.

| Code | Severity | Meaning |
|---|---|---|
| `unsupported_version` | error | `format_version` is not 1 or 2. |
| `empty_tree` | error | The tree has no states. |
| `too_many_states` | error | More than 4096 states. |
| `empty_name` | error | A state has an empty name. |
| `duplicate_name` | error | Two states share a name. |
| `missing_root` | error | `root` names no state. |
| `missing_child` | error | A child entry names no state. |
| `missing_target` | error | A `State` target names no state. |
| `multiple_parents` | error | A state is listed under two parents, or the root is listed as a child. |
| `unreachable_state` | error | The root does not reach the state. |
| `empty_task` | error | A task path is empty. |
| `unknown_task` | error | A task path is not registered (only checked with registered tasks). |
| `no_next_sibling` | error | A `Next` or `NextEnterable` target on a state with no later sibling. |
| `parent_of_root` | error | A `Parent` target on the root. |
| `hold_on_event` | error | An `Event` transition has a `hold`. |
| `bad_value` | error | A non-finite number, a negative hold, an empty event name or parameter key. |
| `condition_too_deep` | error | A condition nests deeper than 16. |
| `bad_include` | error | An include has an empty prefix or path, a `/` in its prefix, or a prefix used twice. |
| `include_missing` | error | An included file cannot be read. |
| `include_invalid` | error | An included file does not parse, or the includes cannot be merged. |
| `include_cycle` | error | A tree includes itself, directly or through other includes. |
| `include_depth` | error | Includes nest deeper than 4. |
| `empty_selection` | warning | A `Utility` or `Weighted` state has no children. |

When none of these fires but the compiler still refuses the tree, the issue carries the compiler's reason (the
list in [state trees](state-tree.md)).

## The panel: `editor.statetree_open`, `editor.statetree_state`, `editor.statetree_edit`, `editor.statetree_save`

```json
{ "method": "editor.statetree_open", "params": { "path": "assets/ai/guard.statetree.ron" } }
{ "method": "editor.statetree_edit", "params": { "label": "alert", "ops": [
    { "op": "rename_state", "name": "Alert", "to": "Alarm" } ] } }
{ "method": "editor.statetree_save", "params": {} }
{ "method": "editor.statetree_state", "params": { "ron": true } }
```

- `editor.statetree_open { path? , create?, root? }` opens a file or starts a new tree (with a root state,
  default `"Root"`). Answers `{ path, states, valid, revision, discarded_unsaved }`.
- `editor.statetree_state { palette?, ron? }` answers the document: `states` in hierarchy order (each with its
  fields, `index`, `depth` and `parent`), `issues`, `valid`, `dirty`, `selected`, and while the game plays
  `agents`, `live { entity, status, active_path, ticks, uses_file, matches_document }` and
  `reload { save, delivered, kept, restarted, rejected, last, reason }`. `palette: true` adds the registered tasks,
  triggers, targets, selections and literal kinds; `ron: true` the text a save would write.
- `editor.statetree_edit { ops?, select?, watch?, label? }` is one undo step (`editor.undo` reverts it, in order
  with level edits). The selection follows a rename; a new state becomes selected. `select: ""` clears it;
  `watch` picks the play-world agent whose active path to show (0: automatic).
- `editor.statetree_save { path? }` writes the canonical text atomically. A tree with errors is refused with
  `invalid_tree` (the issues in `data.issues`) and nothing is written.

### Tree fields, ambient tasks, includes and utility in the panel

The panel's **Tree** section and the inspector edit everything the file has, through the same ops (each click is
one `editor.statetree_edit`-style undo step; a save writes the same canonical bytes `ai.statetree_edit` writes for
the same ops):

| Control | Op |
|---|---|
| Description, Seed, State key, **Apply tree fields** | `set_tree` with only the changed fields. |
| Task path (or a registered task) + **Add ambient task**; **Remove ambient** on a row | `set_ambient` with the new list. |
| Prefix + path + **Add include** | `add_include`. The included states are unreachable (an `unreachable_state` error) until their root is listed under a state: select one and press **List under `<state>`** on the include row (`reparent_state { name: "<prefix>/<included root>", parent }`; the root is read from the included file, so an unreadable one answers `include_missing` / `include_invalid` in the status line). The picker beside the button places it among the parent's children: **at the end** (no `index`) or **before `<child>`** (`index` of that child), one undo step either way (w10). |
| Prefix / path fields + **Change include**; **Remove include** | `set_include` (a new prefix is carried into every reference); `remove_include`. |
| Inspector: Utility key, scale, bias, **Set utility**; **Clear utility** | `set_state { utility }`; `set_state { clear_utility: true }`. |

An include that leads back to the open file is listed as an `include_cycle` error; Save is disabled and
`editor.statetree_save` (or a save of any kind) answers `invalid_tree`, as `ai.statetree_edit` does, and writes
nothing. Undo removes the include.

Panel reasons: `no_tree_open`, `missing_param`, `bad_path`, `open_failed`, `bad_ops`, `too_many_ops`,
`unknown_state`, `invalid_tree`, `save_failed`, `path_outside_project` (ADR-1816), the edit reasons above, and
`F0003` not-found for a file that does not exist.

## Saving while the game plays

An agent whose tree came from a file carries `StateTreeSource { path }`. When the editor saves that file during a
play session it tells the play world the file changed; the play world compiles the file **once** for every agent
that names it (the first agent gets the compiled tree, the others trees built from its merged definition, so 100
agents cost one file read and one include load per save) and swaps the new trees in at the start of the next
fixed step (ADR-2408). Each agent's outcome is `Kept` when every active state still exists by name (the active
path, tick count and event queue carry over) or `Restarted` when an active state is gone (the tree starts again
from the root). `editor.statetree_state` shows the counts and the latest outcome in `reload`; a file the play
world refuses is `rejected` with the reason.

A repeated report of one write (the editor's message plus the asset watcher's) compiles once: the reload
remembers the text it last compiled per file (`StateTreeReloads::unchanged` counts the skips;
`file_compiles` and `instances` count the work).

## Dogfood

```text
editor.statetree_open { "path": "assets/ai/guard.statetree.ron" }
editor.play {}
editor.statetree_state {}                       # live.active_path == ["Root", "Idle"]
editor.statetree_edit { "ops": [{ "op": "add_state", "name": "Search", "parent": "Root" }] }
editor.statetree_save {}
play.wait_until { ... }                         # a frame or two
editor.statetree_state {}                       # reload.last == "kept"
editor.statetree_edit { "ops": [{ "op": "remove_state", "name": "Idle" }] }
editor.statetree_save {}
editor.statetree_state {}                       # reload.last == "restarted"
```
