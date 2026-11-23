# Theme

The **Theme** tab edits a runtime UI theme, a `.theme.ron` file: the colors, sizes, durations and
other tokens your game's interface is drawn with. Open a built-in theme or a theme file, change
tokens, watch a swatch preview and the lint findings, and save. It edits a working copy (the draft)
and never touches the running game's theme; the file it saves is what a game loads. The editor's own
window stays on the editor's tokens. Depth: [Theme panel (agents)](../../agents/ui-theme-panel.md) and
the theme format in [Runtime UI themes](../../agents/ui-theme.md).

## Open it

The tab is in the default layout's bottom group; **View ▸ Reset layout** brings back a closed one.
An agent brings it forward with `editor.layout_apply {"open": "theme"}`. There is no palette command
for this tab.

## Edit a theme

1. Open the **Open theme…** drop-down and pick a theme, or type a path such as
   `path/to/my.theme.ron` in the path field and press **Open file**.
2. To start your own, pick the theme to extend, type a name in **new theme name** and press
   **New child**. The child sets only the tokens you change and inherits the rest.
3. Choose **Comfortable** or **Compact** density.
4. Edit tokens. Rows are grouped (the **Color** group opens first) and a **filter tokens** box narrows
   them. A color row has a color picker, a number row a drag value, any other row a text box
   (an expression such as `@color.accent` or `lighten(@color.accent, 0.06)`); the edit applies
   when the box loses focus. Hover a row's name to see whether it is set here or inherited; **reset**
   removes your value.
5. An edit that would make the theme invalid is refused and the draft stays as it was; the reason shows
   in red under the path field.
6. The swatches above the token list preview the main roles. Under the list, **Lint** says `clean` or
   lists findings with their code (`contrast`, `touch_target`, `unknown_key`, `resolve_error`) and
   severity color.
7. Type the output path and press **Save**. The line `unsaved changes` shows until you do.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| Read the draft and its token rows | The tab | `editor.theme_panel_state` |
| Open a built-in theme, a file, or a new child | **Open theme…**, **Open file**, **New child** | `editor.theme_panel_open` |
| Set or reset tokens, rename, set density | Token rows, density buttons | `editor.theme_panel_set` |
| Lint | The lint section | `editor.theme_panel_lint` |
| Save | **Save** | `editor.theme_panel_save` |

## Keyboard

The tab handles no keys of its own; reach its controls with Tab. A token text box applies its edit
when it loses focus.

## Known limits

- The panel changes a draft and a saved file only; to see a theme in a running game, load the file in
  the game.
- The tab shows no Undo of its own; **reset** puts a token back to inherited.

See also: [UI designer](ui-designer.md), [Editor accessibility](accessibility.md),
[Editor guide](index.md).
