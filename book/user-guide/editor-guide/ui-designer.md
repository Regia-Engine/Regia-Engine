# UI designer

The **UI Designer** tab edits a game UI document (`.ui.ron`) as you see it: a widget palette, a
hierarchy, a live preview, an inspector for the selected widget, bindings, components and a lint
list. The **UI Animation** tab (also opened by the designer's **Animations** button) edits a
`.uianim.ron`: animations, tracks and keys, with scrub and play. Edits share one undo history
with the agent methods. Depth: [UI designer](../ui-designer.md), [UI designer
(agents)](../../agents/ui-designer.md), [UI animation editor](../../agents/ui-anim-designer.md).

## UI Designer tab

| Action | Control / key | Agent method |
|---|---|---|
| Open, new, save | **Open**, **New**, **Save** (**Save*** with edits) | `editor.ui_designer_open`, `editor.ui_designer_save` |
| Undo, redo | **Undo**, **Redo**; Ctrl+Z, Ctrl+Shift+Z / Ctrl+Y while the pointer is over the panel | `editor.undo`, `editor.redo` |
| Add a widget | **Add**, or drag a palette chip into the hierarchy | `editor.ui_designer_edit` |
| Duplicate, remove | **Duplicate**, **Remove** | `editor.ui_designer_edit` |
| Reorder | **Up**, **Down**, or drag a hierarchy row | `editor.ui_designer_edit` |
| Select | Click in the preview or the hierarchy | `editor.ui_designer_state` |
| Bindings | **Add binding** | `editor.ui_designer_edit` |
| Components | **Make component**, **Enter**, **Return** | `editor.ui_designer_make_component`, `editor.ui_designer_enter_component`, `editor.ui_designer_return` |
| Lint, preview | The lint list; **Pseudo** for pseudo-localized text | `editor.ui_designer_lint`, `editor.ui_designer_preview`, `editor.ui_designer_render` |
| List the document's images and masks | the inspector's image fields | `editor.ui_designer_images` |

## UI Animation tab

| Action | Control / gesture | Agent method |
|---|---|---|
| Animations | **Create**, **Rename**, **Delete** | `editor.ui_anim_edit`, `editor.ui_anim_list` |
| Tracks | **Add track**, **Remove track** | `editor.ui_anim_edit` |
| Keys | Click an empty spot of a curve to add, drag to move, **Delete key** | `editor.ui_anim_set_key`, `editor.ui_anim_remove_key` |
| Play | **Play** / **Pause**, **reverse**, the time slider | `editor.ui_anim_scrub` |
| Save, validate | **Save** | `editor.ui_anim_save`, `editor.ui_anim_validate` |

## Keyboard and accessibility notes

Every designer action except dragging palette chips has a button that takes focus with Tab
(**Add** replaces the drag). Moving animation keys needs the mouse; `editor.ui_anim_set_key`
is the keyboard-free route. Findings: [Editor accessibility](accessibility.md).

A canvas with direct manipulation and several widgets selected at once is on the branch
`lane/sprint-v06-ui-designer-canvas`; this page gains it at the v0.6 integration.
