# Agent Changes

Panel id: `agent_changes`.

When an agent wants to change project files, it does not write them. It **proposes** a set of text
files with a reason, and the proposal waits in the **Agent Changes** tab. You read each file's diff
and accept or reject. Nothing changes in the project until you accept. An agent cannot approve its
own work: the `review` permission is in no default profile. Depth: [Staged review
(agents)](../../agents/staged-review.md).

## Open it

The tab is part of the default layout (the group under the viewport); **View ▸ Reset layout**
brings it back when closed. An agent uses `editor.layout_apply {"open": "agent_changes"}`. The
command palette has no `open` command for it.

## Review a proposal

1. Open the tab. The header counts the changes pending and shows the last status line. With
   nothing waiting it reads "No agent changes wait for review."
2. The left column groups changes by set. Each set starts with its number and title, the client
   that proposed it (with its model when known), and **Accept set** and **Reject set** buttons.
   Under it each change is a row with its kind, project path and the lines added and removed.
3. Click a row to select it. The right column shows the path, the **Reason** the agent gave, the
   paths of any before and after screenshots it attached, and the unified diff (added lines green,
   removed lines red, hunk headers blue).
4. Optionally type a reviewer **Note**. It is stored with the decision and cleared afterward.
5. Press **Accept** or **Reject** for the selected change, or the set buttons for the whole set.
   Accepting writes each file atomically if it still holds the text the agent saw when it
   proposed. A file that changed since is a conflict: its change stays pending and the status line
   counts it, so ask the agent to propose again. Accepting also stamps the file's `.meta`
   provenance as reviewed by the owner.
6. Each accepted change is one undo step, described as "Accept agent change to" and the file's path:
   **Ctrl+Z** puts the file's old text back (or removes a file the change created).

A proposal for a golden image stays pending: the status line says to accept it at a terminal with
`regia golden accept` and the change id.

| Action | Control / key | Agent method |
|---|---|---|
| Read the queue and the selected diff | the tab | `editor.agent_changes_state` |
| Select a change | click its row | `editor.agent_changes_select` |
| Accept a change or a set | **Accept**, **Accept set** | `review.accept` (needs the `review` permission) |
| Reject a change or a set | **Reject**, **Reject set** | `review.reject` |
| List pending changes and read a diff | the tab | `review.list`, `review.diff` |
| Propose a change set (the agent's side) | none | `review.propose` |

## Keyboard

The tab has no shortcuts of its own; Ctrl+Z undoes an accepted change like any edit. Rows and
buttons take focus with Tab.

## Known limits

- Only text files can be proposed, and files under `.regia/` and `.git/` are not reviewable.
- The tab decides as the owner at the editor; an agent that holds `review` can accept with
  `review.accept`, which the owner should grant only to a review bot that acts for them.
- The queue is saved in the project, so a proposal waits across editor restarts.

See also: [Editor guide](index.md).
