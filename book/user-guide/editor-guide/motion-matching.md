# Motion matching

<!-- index: Motion matching -->
<!-- index: Motion database -->

Panel id: `motion_matching`. The **Motion Matching** tab is the debug view for one character that has a `MotionMatching` component: what is playing, the latest search, and the database that character uses. The runtime chapter is [Motion matching](../motion-matching.md). The agent methods for this tab are on [Motion matching (agents)](../../agents/motion-matching.md) under the editor panel, and `editor.mm_panel_get`, `editor.mm_panel_select` and `editor.mm_panel_freeze`.

The tab is in the default layout's bottom group. The Animation workspace opens it with the other animation tabs. **View ▸ Reset layout** restores a closed one. An agent opens it with `editor.layout_apply {"open": "motion_matching"}`.

## Read a search

1. Press Alt+P so the character is simulating. The **Character** list names each entity that has the component, the database it uses and the frame count.
2. Pick a character. The line under the list reads the playing clip and time, the search count, the jump count and the search method.
3. The table lists the top candidates (default 8, from 1 to 32). Columns are status, clip, time, frame, total cost and a stacked bar of the cost groups. A bar segment is cheaper or equal (`below`), dearer (`above`) or ignored (the group's weight is 0).
4. **Freeze** holds this report while the game keeps playing. Press it again (**Frozen**) to follow the live search.
5. **Cost parts** lists the same numbers beside the bars. **Contacts and phase** draws one cell per frame and foot when the database has contact labels, with the current frame marked.
6. **Database** shows the asset path, frame count, dimensions, clips and tags. **Sort** orders the clip list and the tag list; selecting the same sort again reverses it. **Edit database** opens that file in the motion database editor.

## The database editor window

**Edit database**, or opening a `.motiondb.ron` from the Content browser, opens the motion database editor. That editor is a window over the dock, not a second dock panel and not a second panel id. It edits the database document (schema, clips, tags) and can rebuild it in memory. Agent methods: `editor.mm_db_open` and the other `editor.mm_db_*` calls ([Motion database editor](../../agents/motion-database-editor.md)). Document autosave keeps an unsaved database (kind recorded with the other editor documents) and offers it after a crash.

## Actions

| Action | Control | Agent method |
|---|---|---|
| Choose the character | **Character** | `editor.mm_panel_select` |
| How many candidates | **top** | `editor.mm_panel_get` (the `candidates` count) |
| Hold the report | **Freeze** | `editor.mm_panel_freeze` |
| Open the database file | **Edit database** | `editor.mm_db_open` |

## Keyboard

The combo box, the candidate count and **Freeze** are focusable. The tab does not take letter keys of its own.

## Known limits

- With no `MotionMatching` entity the character list is empty and the table has nothing to show.
- Freeze keeps the report on screen. It does not pause the game.
- The candidate table stops at 32 rows, which is the matcher's top-k limit.
- The database editor window is not saved as a layout tab. Closing it leaves the **Motion Matching** tab where it was.

## See also

[Animation panels](anim-graph.md), [Play-in-editor](play-in-editor.md), [Motion matching](../motion-matching.md), [Editor guide](index.md).
