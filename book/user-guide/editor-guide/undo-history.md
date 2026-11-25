# Undo History

Panel id: `undo_history`.

The **Undo History** tab lists the editor's shared undo history: every retained edit, oldest first, with the
documents it touched. Click an entry to go back (or forward) to that point; the details under the list say what
the entry changed. It is the same history Ctrl+Z and Ctrl+Shift+Z walk, and the same history an agent reads with
`editor.history`. Depth: [Shared undo history and recovery (agents)](../../agents/undo.md), [Undo and redo in the
editor (agents)](../../agents/undo-redo.md).

## Open it

Choose **Window ▸ Undo History**. From the keyboard press Ctrl+Shift+P and pick **Open the Undo History tab**
(`window.open_undo_history`); bind a key to it in the keymap if you use it often. An agent brings the tab
forward with `editor.layout_apply {"open": "undo_history"}`.

## Read the history

1. The first line counts the entries: "N applied · M available to redo".
2. The list starts with **0 · Before retained history** (the state before the oldest edit the editor still keeps),
   then one row per edit: its number, its description and the documents it touched (`level` is the open level).
   The last applied edit is marked **Current**; entries after it, which a redo would apply again, are marked
   **Redo**.
3. Hover a row, or move keyboard focus to it, to show its **Change details**: for each thing the edit touched, the
   document and, when the edit reports them, the entity, its guid, the asset and the property. "This command does
   not report detailed changes" means an older kind of edit has not supplied details.
4. While a script or agent transaction is open, a line says "Transaction in progress" with its label, and the rows
   are disabled: jumps wait until the transaction is committed or aborted.

## Jump to a point

Click a row, or Tab to it and press Enter or Space. The editor undoes or redoes one edit at a time until exactly
that many entries are applied: **0** undoes everything retained, the last row redoes everything. Each edit's own
revert or apply runs, so a jump is the same as pressing Ctrl+Z or Ctrl+Shift+Z that many times. A refused jump
shows its error under the details.

Making a new edit after jumping back drops the entries marked **Redo**, as Ctrl+Z followed by an edit does. The
history keeps a bounded number of entries; the oldest fall off, and row numbers always count the retained history.

## Actions and agent methods

| Action | Control / key | Agent method |
|---|---|---|
| Read the history with its details | The list and **Change details** | `editor.history` |
| Jump to an entry | Click a row, or Tab to it and press Enter or Space | `editor.history_jump {"index": N}` |
| Undo, redo | Ctrl+Z, Ctrl+Shift+Z or Ctrl+Y | `editor.undo`, `editor.redo` |
| Group several edits into one entry | (agents and scripts) | `editor.transaction_begin`, `editor.transaction_commit`, `editor.transaction_abort` |
| Capture the tab as an image | (agents) | `editor.capture_panel {"panel": "undo_history"}` |
