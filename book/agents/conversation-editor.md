# Conversation graph editor

Conversations (`assets/conversations/<id>.conv.ron`, see [Conversations, captions and subtitles](conversation.md)) are
lists of steps: `Line`, `Label`, `Goto`, `If`, `Choice`, `Event` and `End`. The editor's **Conversation** panel shows
one as a graph so a writer can see every branch, edits it with undo, draws the validator's findings on the nodes and
previews a run from any node. The `editor.conversation_*` methods drive the same panel (ADR-3298). The file format
did not change.

## The graph

A **block** node holds `Label`, `Line` and `Event` steps. A block starts at the first step, at every `Label` and
after every step that leaves (`Goto`, `If`, `Choice`, `End`); a `Goto` ends the block it is in. `If`, `Choice` and
`End` are nodes of their own. Edges:

| Kind | From | Meaning |
|---|---|---|
| `next` | a block | Falls through to the following step. |
| `goto` | a block ending in `Goto` | The jump. |
| `then`, `else` | an `If` node | The two branches; `cond` is the condition. |
| `option` | a `Choice` node | One edge per option, with its `option` index, `text` key and `cond`. |

A jump to a label that does not exist is an edge with `to: null` (drawn as a red dangling arrow).

Node ids are stable names: `label:<name>` for a block that starts at a label, `line:<id>` for one that starts at a
line, and `<kind>:<n>` (the n-th node of that kind without a natural name: `if:1`, `choice:2`, `end:1`, `block:1`)
otherwise. Node 0 is the entry.

### Layout

The layout is automatic and layered: a node's layer is its breadth-first distance from the entry (nodes the entry
cannot reach start their own walk below), and nodes inside a layer are ordered by the mean place of their parents in
the layer above, ties by step order. It reads only ids and edges, so editing a line's text, speaker or note moves
nothing. A dragged node keeps its place in an editor-side sidecar,
`<project>/.regia/conversation_layout/<file name>.layout.ron` (`format_version: 1`, `positions: { node id: (x, y) }`),
written when the conversation is saved. The conversation file never stores positions.

## Files and undo

Opening keeps the file's bytes: saving without edits writes exactly what was read. Every edit writes the asset's
canonical form (the serializer `regia_framework::conversation::asset::to_ron` uses) and is one entry on the editor's
undo stack, a snapshot of the file text and the sidecar, so `editor.undo` (or Ctrl+Z) after an edit restores the
exact bytes before it. A file that does not parse opens **read-only** with the parse error (`file:line:col:
message`); edits and preview are refused with `read_only` until the file is fixed and opened again.

## Methods

| Method | Does |
|---|---|
| `editor.conversation_open { path, project? }` | Opens a `.conv.ron` (project-relative, or absolute under the project or the temp dir; `path` and `project` follow the document rule of [Request paths](safety.md#request-paths), ADR-7388: `path_outside_project` otherwise). `project` is the root holding `assets/` (default: the editor's project, else the directory above `assets/`). Answers `{ path, id, read_only, parse_error, nodes, edges, issues, revision, discarded_unsaved }`. |
| `editor.conversation_graph { search?, text? }` | The graph: `nodes` (`id`, `kind`, `label`, `first_step`, `steps`, `x`, `y`, `pinned`, `layer`, `issues`), `edges` (`from`, `to`, `kind`, `step`, `label`, `option`, `cond`, `text`), `issues` (each finding with its `node`), `entry`, `selected`, `dirty`, `revision`; `matches` (node ids) for a `search`; the file `text` with `text: true`. |
| `editor.conversation_edit { ops?, move?, select?, label? }` | One undoable edit: `ops` all or nothing (below); `move: { node, x, y }` pins a node in the sidecar, `move: { node, reset: true }` returns it to the automatic layout; `select` picks a node id (`""` clears). Answers `{ revision, dirty, nodes, edges, issues, selected }`. |
| `editor.conversation_save { path? }` | Saves the file (and the sidecar when it has positions). Answers `{ path, sidecar }`. |
| `editor.conversation_preview_step { start?, conditions?, choose?, locale?, play?, stop? }` | Steps a preview (below). Answers `{ state, node, step, line?, options?, voice?, events, trace, locale, reason?, detail? }`. |

Each step in `nodes[].steps` is `{ index, kind, ... }`: `line` (`line`, `speaker`, `text`, `display` = the text in
the source locale), `label` (`name`), `goto` (`label`), `if` (`cond`, `then`, `else`), `choice` (`prompt`,
`options`), `event` (`name`) or `end`.

## Edit operations

Step indices are into the conversation's steps (`first_step` + position in `steps`). A conversation without a
`steps` list plays every line in file order; its first step edit writes that list out, which plays the same.

| `op` | Fields | Does |
|---|---|---|
| `add_line` | `id`, `speaker`, `text`, `at?` | Adds a line and a `Line` step for it at `at` (default: the end). |
| `add_step` | `step`, `at?` | Inserts a step: `{ "kind": "line", "line" }`, `{ "kind": "label", "name" }`, `{ "kind": "goto", "label" }`, `{ "kind": "if", "cond", "then", "else" }`, `{ "kind": "choice", "prompt"?, "options": [{ "text", "cond"?, "goto" }] }`, `{ "kind": "event", "name" }`, `{ "kind": "end" }`. |
| `delete_step` | `at` | Deletes a step; a deleted `Line` step's line goes too when no other step plays it. |
| `move_step` | `from`, `to` | Reorders. |
| `set_line` | `line`, `text?`, `speaker?`, `note?`, `emotion?` | Changes a line (`emotion: ""` clears it). |
| `set_prompt` | `at`, `prompt` | A choice's prompt key (`""` clears it). |
| `add_option` | `at`, `text`, `cond?`, `goto` | Adds an option to a choice. |
| `set_option` | `at`, `option`, `text?`, `cond?`, `goto?` | Changes an option (`cond: ""` clears the condition). |
| `delete_option`, `move_option` | `at`, `option` / `at`, `from`, `to` | Removes or reorders an option. |
| `set_cond` | `at`, `cond` | An `If`'s condition. |
| `set_event` | `at`, `name` | An `Event`'s name. |
| `retarget` | `at`, `branch?`, `option?`, `label` | Points a jump at another label: `branch` is `goto` (default), `then`, `else` or `option` (with `option`). The panel's drag from an edge port onto a node is this op. |
| `rename_label` | `from`, `to` | Renames a label and every `Goto`, `If` branch and option that names it, as one step. |

```json
{ "method": "editor.conversation_edit", "params": { "label": "add a branch", "ops": [
    { "op": "add_line", "id": "think", "speaker": "hero", "text": "gate-think", "at": 1 },
    { "op": "retarget", "at": 2, "branch": "then", "label": "long" },
    { "op": "rename_label", "from": "short", "to": "quick" } ] } }
```

## Findings

The findings are the `conv_*` validator's ([codes](conversation.md)), run on the open conversation against the
project's `speakers.ron`, `conversation.ron`, every locale under `assets/localization/` and the clips under
`assets/`, plus one editor check: `conv_empty_choice` (error), a `Choice` with no options, which ends the run. Each
finding names the node it is drawn on: an unreachable label or line (`conv_unreachable`), a missing jump target (the
node that jumps, `conv_bad_label`), a missing voice clip per locale (`conv_missing_clip`), a line too long for its
subtitle budget (`conv_line_too_long`), and the rest. Project-level findings (`conv_speaker_contrast`) have no
node. Clicking a finding in the panel selects and centres its node.

## Preview

`start` begins a run at a node id (`""` is the entry node) and runs to the first line, choice or end. Without
`start` the preview moves past the shown line, or takes option `choose` (an index among the offered options) of the
waiting choice. The rules are the runner's: `conditions` is the mocked condition table (a name not listed is unknown,
and unknown is false); a choice offers the options whose condition is absent or true and, when none is, takes the
first option without a condition or else ends; more than `max_steps_per_frame` non-blocking steps in a row stops the
preview with `reason: "step_budget"`. `state` is `line`, `choice`, `finished` or `stopped`. A line answers its
`voice.clip` (resolved through the voice path template in `locale`, default the source locale); `play: true` also
plays it on the editor's preview output (`voice.played`). `events` lists the `Event` steps passed, `trace` every
step executed with its node.

## Errors

Every error is `F0002` with `data.reason`:

| Reason | When |
|---|---|
| `no_conversation_open` | A method other than `open` before a conversation is open. |
| `read_only` | An edit or preview of a file that does not parse. |
| `bad_path`, `io_error`, `missing_param`, `bad_param` | A path that is not a `.conv.ron`, an unreadable file, a missing or mistyped parameter. |
| `bad_ops`, `too_many_ops` | `ops` is not a list of known ops, or has more than 512. |
| `bad_step`, `wrong_step`, `bad_option`, `bad_node`, `bad_move` | An index or id that does not exist, or names the wrong kind of step. |
| `unknown_line`, `duplicate_line`, `unknown_label`, `duplicate_label`, `empty_name` | A line or label that is missing or already exists, or an empty name. |
| `serialize_failed` | The edited asset could not be written (a bug). |
| `save_failed` | No path, a path that is not a `.conv.ron`, or a failed write. |
| `no_preview`, `choice_required`, `choice_out_of_range`, `no_choice_pending`, `finished`, `locale_not_loaded` | A preview step that does not fit its state, or a locale the project does not have. |
