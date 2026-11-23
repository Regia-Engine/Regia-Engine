# Notes and Activity

Two tabs for reviewing a level together. **Notes** holds review notes pinned in the world: a note is a marker in the
open level with a text, its author, replies and a resolved flag, saved with the level and stripped from cooked and
packaged levels. **Activity** is the session's timeline of who changed what: every agent call and every user edit,
undo and redo, in the order they happened. Depth: [Review notes and session Activity
(agents)](../../agents/review-notes.md).

## Open them

Choose **Window ▸ Notes** or **Window ▸ Activity**. From the keyboard press Ctrl+Shift+P and pick **Open the Notes tab** or
**Open the Activity tab** (`window.open_notes`, `window.open_activity`). An agent runs the same commands with `editor.command`.

## Notes

1. The first line shows the author name new notes get: the project's git `user.name`, else the `review.author`
   preference.
2. **Show** filters the list: **all**, **open**, **resolved** or **mine** (notes whose author is you).
3. To add a note, type in **New note** and press **Add note**. The note goes where you placed it in the viewport
   (press **N** over a surface to start a note there), else on the selected entity. Without either, the tab shows an
   error under the list and adds nothing.
4. Each row reads "author: text (N replies)". Click it to frame the note in the viewport and open it: its replies
   are listed, with a **Reply text** box, **Reply**, and **Resolve** (or **Reopen** on a resolved note).
5. Every change is one undo entry and marks the level unsaved; nothing is written until you save the level.

A note badge in the viewport shows how many messages the thread has; clicking it frames the note as well.

## Activity

1. **Source** picks **all**, **agent** (calls an agent made) or **user** (your edits, undos and redos).
2. **Method filter** keeps the rows whose method or command contains the text (case-sensitive).
3. Each row reads "time (Unix milliseconds) · who · method or command · outcome". An agent row names the
   connection's client; a user row names the edit. Below a row, **Select entity N** selects an entity the row touched (disabled once that entity is
   gone), and the asset paths it touched are listed.

The tab keeps the newest 4,000 rows. It also reads the end (the last 1 MiB) of the project's existing agent audit
log, so calls from earlier sessions appear, without their targets (that log did not store them).

## Actions and agent methods

| Action | Control / key | Agent method |
|---|---|---|
| Start a note at the surface under the cursor | **N** in the viewport (command `review.add_note`, rebindable) | `editor.note_add {"text", "position"}` |
| Add a note | **New note**, **Add note** | `editor.note_add` |
| List notes with a filter | **Show** | `editor.note_list {"filter"}` |
| Frame a note | Click its row or its badge | `editor.select`, `editor.focus` |
| Reply | **Reply text**, **Reply** | `editor.note_reply` |
| Resolve or reopen | **Resolve**, **Reopen** | `editor.note_resolve {"resolved"}` |
| Read the activity timeline | **Source**, **Method filter** | `editor.activity {"since", "source", "method"}` |
| Select an entity from a row | **Select entity N** | `editor.select` |

Tab and Shift+Tab reach every filter, text box and button; Space or Enter presses the focused button.
