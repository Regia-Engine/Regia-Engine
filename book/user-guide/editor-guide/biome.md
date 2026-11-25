# Biome

Panel id: `biome`.

The **Biome** tab edits a biome definition (`.biome.ron`): what the procedural tools grow in an area and how. It shows
the file as forms: the biome's name, color, map key and priority; its layers (rank, sharing, the candidate source and
its density noise, channels and their rules, child depth); each layer's entries (what spawns, its weight, the channels
it takes, its filters with a small curve of their density window, footprint, placement, render settings and children);
and the grass multipliers. Depth: [Biome editor (agents)](../../agents/biome-editor.md), [Biomes](../../agents/biomes.md)
(what each field does).

## Open it

Choose **Window ▸ Biome**, or press Ctrl+Shift+P and pick **Open the Biome tab** (`window.open_biome`). To make a new
biome, use **Content ▸ New ▸ Biome**: it creates a file named after itself with no layers. The procedural **Area**
tool picks its biome from the project's `.biome.ron` files.

## Edit a biome

1. Pick a `.biome.ron` of the project from the list, or type its path, and press **Open**; type a new path and press
   **New** to start a biome there. A file that breaks a rule still opens, and its issues are listed.
2. Edit the forms: numbers drag, switches toggle, choices are drop-downs, and a text field takes effect when you press
   Enter or leave it. Every list has ▲ and ▼ to reorder, **Remove** and **Add**.
3. An edit that would break a rule (a duplicate name, a negative weight, a filter window whose minimum is above its
   maximum, a child naming no entry of its layer) is refused and changes nothing, so the open biome always stays one
   the game can load. Renaming an entry renames the children that name it; renaming a channel renames it in the
   layer's entries.
4. **Undo** and **Redo** keep 128 steps, one per edit; one drag of a value is one step.
5. **Save** writes the file in its canonical form. RON comments in the opened file are not kept: when the file has
   any, a warning line says how many will be lost. A file with rule issues saves only once they are fixed.

Opening another file or starting a new biome while the open one has unsaved edits is refused first: the tab shows the
refusal with a **Discard edits and open …** (or **… start …**) button, and **Keep editing** keeps your edits. The tab has
no autosave, so save first.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| Open a biome, or start a new one | **Open**, **New**; **Content ▸ New ▸ Biome** | `editor.biome_panel_open {"path", "new"?, "discard"?}`, `editor.create_asset {"kind": "biome", "path"}` |
| Read the definition, its issues and the undo depth | The forms and the issue list | `editor.biome_panel_get` |
| Change a field, add to or remove from a list, move an item | The forms, **Add**, **Remove**, ▲ ▼ | `editor.biome_panel_edit {"op", "path", "value"?, "index"?, "to"?, "merge"?}` |
| Undo, redo | **Undo**, **Redo** | `editor.biome_panel_edit {"op": "undo"}`, `{"op": "redo"}` |
| Save | **Save** | `editor.biome_panel_save` |
