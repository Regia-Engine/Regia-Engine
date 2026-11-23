# Staged review

The owner reviews what an agent wants to change before it lands (specs/23 § 5, ADR-2346). An
agent **proposes** a change set of text files; nothing in the project changes until a human
accepts it in the editor's **Agent Changes** panel (or a client holding the `review` permission
calls `review.accept`). Accepted files are written atomically and their `.meta` provenance is
stamped `reviewer: owner`, which the shipping cook gate below checks.

## Proposing: `review.propose`

```sh
regia agent call review.propose '{
  "changes": [{ "path": "materials/door.material.ron", "text": "(\n    base_color: (0.8, 0.3, 0.2, 1.0),\n    roughness: 0.5,\n)\n" }],
  "reason": "warmer door color, as asked",
  "images": { "before": "captures/door_before.png", "after": "captures/door_after.png" }
}'
```

<!-- not-a-request -->
```json
{
  "set": 1,
  "changes": [{
    "id": 1, "path": "materials/door.material.ron", "kind": "modify", "staged": true,
    "added": 1, "removed": 1,
    "diff": "@@ -1,4 +1,4 @@\n (\n-    base_color: (1.0, 1.0, 1.0, 1.0),\n+    base_color: (0.8, 0.3, 0.2, 1.0),\n     roughness: 0.5,\n )\n"
  }],
  "persisted": true
}
```

- `changes` holds 1..=64 `{ path, text }`: a project-relative path (the project-relative rule of
  [Safety](safety.md#request-paths)) and the file's full proposed text (at most 1 MiB, UTF-8).
  A missing file is a `create`; an unchanged one is answered `kind: "unchanged"` and not staged.
- `reason` is required: the reviewer reads it next to the diff. `title` names the set (default:
  the reason). `images` attaches before/after screenshots (project-relative, e.g. written by
  `seeing.screenshot`), shown in the panel.
- `dry_run: true` answers the diffs and stages nothing.
- Each change remembers the file's text at proposal time. The queue persists in
  `.regia/agent/review/queue.json` (format version 2; a version 1 file still loads), so a proposal waits across editor restarts.
  At most 256 changes wait at once. An existing file over 1 MiB cannot be proposed
  (`review_too_large`). The newest 32 decided changes keep their texts; older decided changes keep
  their summary only (`texts_dropped: true`, no diff).
- Files under `.regia/` and `.git/` are not reviewable (`review_path_denied`). Permission: `assets`.

## Reading the queue

```sh
regia agent call review.list '{"status": "pending"}'
regia agent call review.diff '{"id": 1, "full": true}'
```

`review.list { status?, set? }` answers summaries (`id, set, path, kind, status, title, reason,
client, model, created_ms, added, removed, images, decided_by, note`) and the pending count; status
is `pending` (default), `accepted`, `rejected` or `all`. `review.diff { id, full? }` answers the
unified diff with a `--- a/<path>` / `+++ b/<path>` header (`--- /dev/null` for a new file) and, with
`full: true`, both texts. The diff aligns lines by their longest common subsequence; a changed
middle larger than 4 million line pairs is shown as one replaced block.

## Deciding

```sh
regia agent call review.accept '{"set": 1, "note": "looks right"}'
regia agent call review.reject '{"ids": [2], "note": "wrong file"}'
```

Name the changes with exactly one of `ids`, `set` (its pending changes) or `all: true`.

- `review.accept` (permission `review`) applies each change whose file still holds its
  proposal-time text; a file that moved on answers `review_conflict` for that change, which stays
  pending (ask the agent to propose again). Each result says `meta: stamped` (the `.meta`
  provenance now says `reviewer: owner`, plus the proposing client, session and model when the
  record lacked them), `no_meta` (the file has no sidecar yet) or `failed`.
- `review.reject` (permission `assets`) drops changes unapplied. Without the `review` permission
  an agent withdraws only its own changes (same client and session): `set` and `all` select its own
  pending changes, and a named id another agent proposed answers `review_not_yours` in its row.

**An agent cannot approve its own work:** the `review` permission is in no default profile, and
no tool scope can hold it (like `tools` and `build`). The owner accepts in the editor's Agent Changes
panel, which decides in-process. Grant `review` in `.regia/agent.toml` only to a client that acts
for the owner (a review bot the owner runs).

## The shipping gate

```toml
# Regia.toml
[provenance]
require_review_for_shipping = true
```

With this, `regia cook --quality final` and `regia package` (also when its paks are fresh) refuse while any asset's `.meta` says `tool: agent` or
`tool: generator` with `reviewer: pending` (a `.meta` that cannot be read counts as unreviewed),
naming the first 20. The editor stamps agent imports `tool: agent, reviewer: pending`; staged review
stamps accepted files `reviewer: owner`. `--quality fast` cooks are not gated.

## Showing things to the humans watching: `agent.open_in_editor`

```sh
regia agent call agent.open_in_editor '{"asset": "prefabs/door.prefab.ron"}'
regia agent call agent.open_in_editor '{"entity": 4294967301}'
regia agent call agent.open_in_editor '{"file": "src/door.rs", "line": 42, "dry_run": true}'
```

Exactly one of `entity` (select it in the editor), `asset` (a `.prefab.ron` opens in prefab
editing; other asset kinds answer `open_in_editor_unsupported`: open them as a `file`) or `file` +
`line` (opens the owner's code editor; `F0003` when the file does not exist).
The editor host tries the preference `code.editor_command` first, then `[editor] code_editor` in
`.regia/agent.toml`. `{file}` (absolute path) and `{line}` are substituted per argument and the
program runs without a shell. `open_in_editor_no_code_editor` is answered only when both are unset
(`agent_open_in_editor_reads_the_preference_before_the_project_config`):

```toml
[editor]
code_editor = ["code", "-g", "{file}:{line}"]
```

`dry_run: true` answers what would open and the command. Outside the editor the method answers
`editor_not_running`. Permission: `play` (it drives what the humans see; it is rate-limited like
other mutating calls).

## The Agent Changes panel

The editor's **Agent Changes** tab lists the pending changes grouped by set (title, proposing
client and model), each with its kind, path and line counts. Selecting one shows the reason, the
attached screenshot paths and the colored unified diff; **Accept** / **Reject** decide that change,
**Accept set** / **Reject set** the whole set, with an optional note. The panel decides as the owner
(`decided_by: "owner"`), and each change it accepts becomes an editor undo entry ("Accept agent
change to <path>"): undo puts the proposal-time text back (or removes a created file), redo writes
the accepted text again. Its agent surface:

- `editor.agent_changes_state` answers `{ pending, selected, diff, status }`: what the panel shows.
- `editor.agent_changes_select { id }` shows one pending change's diff, as a click does
  (`review_change_not_found` for an unknown or decided id). It needs `edit_scene` and is mutating
  (ADR-3285): it changes `selected` and `diff` on the next `editor.agent_changes_state`.

Agents never decide through the panel's methods; `review.accept` with the `review` permission is
the only protocol path to accept.

A golden proposal (`agent.bless_golden`, [Golden images](goldens.md)) is listed here too, with its reason and images,
but neither `review.accept` nor the panel's Accept applies it: they answer `golden_owner_only` and it stays pending
until the owner runs `regia golden accept <id>` at a terminal. Reject works as usual.

## Reasons

Errors carry `data.code` `F0002` (or `F0003` for an unknown change) and a lowercase `data.reason`;
per-change outcomes of `review.accept` and `review.reject` carry `reason` in their result row.

| Reason | When |
|---|---|
| `review_not_text` | The current file is not UTF-8 text. |
| `review_path_denied` | The path is agent state (`.regia/`), version control (`.git/`), leaves the project, or, in a queue file written by hand, names a golden. |
| `review_queue_full` | 256 changes already wait for review. |
| `review_too_large` | A proposed text, or the file it replaces, is over 1 MiB. |
| `review_change_not_found` | No such change id. |
| `review_already_decided` | The change was accepted or rejected before. |
| `review_not_yours` | `review.reject` without the `review` permission named another agent's change. |
| `review_conflict` | The file changed since the proposal; the change stays pending. |
| `review_write_failed` | Writing the file failed (disk, permissions). |
| `golden_owner_only` | A golden proposal, in a `review.accept` row: only the owner's terminal accepts it. |
| `editor_not_running` | `agent.open_in_editor` outside the editor. |
| `open_in_editor_no_code_editor` | Neither `code.editor_command` nor `[editor] code_editor` names a program. |
| `open_in_editor_unsupported` | The editor cannot open that kind of asset. |
