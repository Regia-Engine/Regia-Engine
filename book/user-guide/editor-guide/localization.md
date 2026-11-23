# Localization

The **Localization** tab shows the project's Fluent translation files as a table: one row per key, one column per
locale (the source locale first), each cell colored by its state. Edit a translation in place, gather the keys the
content and code use, find where a key is used, preview pseudo-localization in the viewport, and hand a locale to a
translator as a PO or XLIFF file and read it back. It uses the same library calls as `regia loc`. Depth: [Editor
localization (agents)](../../agents/editor-localization.md), [Localization](../localization.md), [Localization
(agents)](../../agents/localization.md).

## Opening it

The tab sits in the group under the viewport in the default layout; **View ▸ Reset layout** brings it back if you
closed it. An agent brings it forward with `editor.layout_apply {"open": "localization"}`. It has no palette command
and no key. The locale list itself is not edited here: that is **Project Settings**.

## Walk-through: translate a missing string

1. Open the tab. The header reads the source locale, the number of locales and keys, and each locale has a progress
   bar with its translated, placeholder and stale counts. Click **Reload** to read the files again.
2. Pick **missing** in the first drop-down to list only keys with a missing translation (the others are **all**,
   **untranslated**, **stale** and **long**, for strings longer than the source by a ratio you set with the **ratio**
   box). Pick a locale in the second drop-down, or **every locale**, and use **Search** to narrow by text. **◀** and
   **▶** page through the rows.
3. Click a cell. Its key and locale appear under the table with the source text, and an edit box fills with the
   current text (or the source text when the cell is empty). A cell outlined orange is longer than the source; hover a
   cell to see its state and the file that defines it.
4. Edit the text and click **Apply**. The value is written back into the file that defines it with every other byte
   kept, as one undo step (Ctrl+Z restores the file).
5. Click **Find usages** to list the files that name the key (layouts, scenes, conversations, dialogues, code) and,
   when the editor has an asset registry, the assets that reach them.
6. Click **Gather** to find the keys content and code use and add the missing ones to each locale's `gathered.ftl`;
   a line under the table reports how many keys and entries it found and added. Gathering is one undo step.
7. Tick **Pseudo (viewport)** to preview pseudo-localization of the viewport's runtime localization, in the edit world and
   in play; the label reads **Pseudo (viewport: no runtime localization)** when there is none.

## Walk-through: a translator round trip

1. In the **Exchange** row pick a target locale (not the source), a format (`po` or `xliff`) and a filter (`all`,
   `untranslated` or `stale`). The file box hints `.regia/loc/<locale>.po`; the **…** button picks a file.
2. Click **Export** to write the translator's file.
3. When the file comes back, name it in the box and click **Import…**. The panel shows what would change (added,
   changed, still missing, skipped entries) before it writes anything.
4. Click **Apply (N file(s))** to write the changes as one undo step, or **Cancel**.

## Actions

| Action | Control / key | Agent method |
|---|---|---|
| Read the table | The table, filters, **Search** | `editor.loc_table` |
| Edit one translation | A cell, the edit box, **Apply** | `editor.loc_set` |
| Gather keys | **Gather** | `editor.loc_gather` |
| Find where a key is used | **Find usages** | `editor.loc_usages` |
| Preview pseudo-localization | **Pseudo (viewport)** | `editor.loc_pseudo` |
| Export a locale for a translator | **Export** | `editor.loc_export` |
| Read a returned file | **Import…**, **Apply**, **Cancel** | `editor.loc_import` |
| Read the files again | **Reload** | none (view only) |
| Undo | Ctrl+Z | `editor.undo` |

## Keyboard

The panel handles no keys of its own. Tab and Shift+Tab move between its controls, and Space or Enter presses the
focused button or cell ([Editor guide](index.md)).

## Known limits

- The panel does not edit `localization.ron`: add or remove locales in **Project Settings**.
- Pseudo-localization changes the viewport's runtime localization only, not the files.
- A returned file is planned first; nothing is written until you click **Apply**.

## See also

[Localization](../localization.md), [Conversation](conversation.md), [Editor localization
(agents)](../../agents/editor-localization.md).
