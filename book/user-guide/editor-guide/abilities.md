# Abilities

Panel id: `abilities`.

The **Abilities** tab inspects one entity's gameplay-ability data: its attributes (base value, current value and
how the current value was reached), the effects live on it, and the abilities it has been granted. It reads the
play world while a play session runs and the edit world otherwise, so the same tab shows a level being authored
and a game being played. A collapsible editor at the bottom opens the three ability asset files
(`.attrs.ron`, `.effect.ron`, `.ability.ron`) in a typed form with validation. Depth: [Abilities
(agents)](../../agents/abilities.md), [Editor gameplay data panels](../../agents/editor-data-panels.md),
[Gameplay](../gameplay.md).

## Opening it

The tab sits in the group under the viewport in the default layout; **View ▸ Reset layout** brings it back if you
closed it. An agent brings it forward with `editor.layout_apply {"open": "abilities"}`. The panel has no palette
command and no key.

## Walk-through: give an entity an attribute set and apply an effect

1. Select an entity in the Outliner. With **follow selection** chosen in the entity picker the panel inspects
   it; pick an entity from the same picker to pin it instead.
2. Type an attribute set's path under `assets/` (for example `attrs/hero.attrs.ron`) in the box under the
   attributes and click **Add attributes**. The attributes grid appears with a **name**, **base**, **current**
   and **range** column.
3. Drag a **base** value to change it. Hover the **current** value to read its breakdown (the base, each
   modifier's contribution, clamping). A derived attribute's base is not editable.
4. Open the **effect…** drop-down, pick an effect asset and click **Apply effect**. It appears under **Live
   effects** with its level, stack count, remaining ticks and a suspended mark when suspended; **Remove** takes it off.
5. Pick an ability in the **ability…** drop-down and click **Grant**. It appears under **Abilities** with its state
   (ready, running, ending or cooling) in color. **Revoke** takes it away.
6. Press Alt+P to play. The tab now shows the play world: **Start** runs a granted ability.

In the edit world, **Add attributes** and **Apply effect** are one undo step each (Ctrl+Z puts the entity's
attributes, effects, grants and counted tags back). Operations in the play world are not undoable: the play world is
thrown away at Stop.

## Actions

| Action | Control / key | Agent method |
|---|---|---|
| Pin an entity, or follow the selection | Entity drop-down (**follow selection** or a listed entity) | `editor.abilities_panel_select` |
| Draw the state rings in the viewport | **Overlay** check box | `editor.abilities_panel_select` (`overlay`) |
| Read what the panel shows | The panel | `editor.abilities_panel_get` |
| Change an attribute's base | The **base** drag value | `editor.abilities_panel_call` (`set_base`) |
| Add an attribute set | **Add attributes** | `editor.abilities_panel_call` (`add_attributes`) |
| Apply an effect | **Apply effect** | `editor.abilities_panel_call` (`apply_effect`) |
| Remove a live effect | **Remove** | `editor.abilities_panel_call` (`remove_effect`) |
| Grant an ability | **Grant** | `editor.abilities_panel_call` (`grant`) |
| Revoke a grant | **Revoke** | `editor.abilities_panel_call` (`revoke`) |
| Start a granted ability (play only) | **Start** | `editor.abilities_panel_call` (`activate`) |
| Open, edit and save an asset file | **Asset** section: **Open**, the form, **Save** | `editor.abilities_panel_asset` |
| Show the raw text instead of the form | **Advanced (raw text)** | none (view only) |

## The asset editor

Open the **Asset (.attrs / .effect / .ability)** section, type a path under `assets/`
(for example `effects/burn.effect.ron`) and click **Open**. The file is checked as you edit:

- The typed form has one row per top-level field of the asset, with nested records and lists under it. Numbers
  are drag values, booleans are check boxes, an enum has a variant picker and a list has **Add entry** and
  **Remove entry**. A validation message appears beside the field it names; messages about the whole asset and parse
  errors (with a line and column) appear under the form.
- `format_version` is shown but not edited in the form.
- **Advanced (raw text)** shows the text itself (it is also shown while the text does not parse).
- The title shows the path with a `*` while there are unsaved changes. **Save** is enabled only when the text
  differs from the saved file and validation finds no error, and it writes the file atomically.
- A typed edit writes the format's canonical text, which drops RON comments. A banner above the form warns about
  this before your first typed edit of a file that has comments; edit in **Advanced** to keep them.

## Keyboard

The panel handles no keys of its own. Tab and Shift+Tab move between its controls; the global keys (Ctrl+Z,
Ctrl+Y, Alt+P) work as everywhere ([Editor guide](index.md)).

## Known limits

- The entity list shows at most 256 entities (in id order).
- **Start** works only while playing (it is disabled in the edit world); the tab reads "no ability plugin in this
  world" when the world does not run the ability plugin.
- Play-world entity ids differ from edit-world ids, so the pin is dropped when a play session starts.
- Edits to a base value while playing change the play world only.
