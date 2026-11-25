# VFX Editor

Panel id: `vfx_editor`.

The **VFX Editor** tab edits a `.vfx.ron` particle effect: the list of emitters, each emitter's
module stack (gravity, drag, size over life, ...), curve and gradient editors, a live preview and a
table of what each emitter costs. Every change is one step on the editor's shared undo stack and is
checked by the `.vfx.ron` validator; an edit the validator refuses changes nothing. Depth: [VFX
(agents)](../../agents/vfx.md) and [VFX](../../systems/vfx.md).

## Open it

The tab is part of the default layout (the group under the viewport). If you closed it,
**View ▸ Reset layout** brings it back. An agent brings it forward with
`editor.layout_apply {"open": "vfx_editor"}`. The command palette has no `open` command for this tab.

## Edit an effect

1. Type the project-relative path of an effect (for example `vfx/explosion.vfx.ron`) in the path
   box at the top and press **Open**. The panel does not read a selection from the Content browser.
2. Pick an emitter in the **Emitters** list. **Up** and **Down** reorder emitters, **Remove**
   deletes one (not the last), and the box under the list with **Add emitter** adds a new one with
   the name you typed.
3. Under **Module stack** each module is a collapsible row named `<stage> / <module>`. Open a row
   to edit its parameters. The box in the row header is the A/B switch: unchecking it takes the
   module out of the effect for the preview and the saved file (the row turns gray and reads
   "(disabled)"); checking it puts the exact values back. **Remove** deletes an optional module.
   The `spawn`, `init` and `render` modules are always there: they have no switch and no Remove.
4. To add a module, pick it in the list next to **Add module** and press the button. The list
   offers only the optional modules the emitter does not have yet.
5. Under **Curves** and **Gradients**, double-click an empty spot to add a key, drag a key to move
   it, and right-click a key to delete it (gradients: click a marker to select it, then edit its
   time and color in the key row, or **Delete** it). Curves are linear between keys.
6. Watch the **Preview** on the right. **Play** and **Pause** run the preview clock, **Restart**
   starts over, **seed** and the **time s** slider (0 to 30 s) pick a deterministic frame. Under
   it, **Per-emitter cost** lists, for each emitter, the live particles, spawn rate, draw list,
   draw calls and GPU time.
7. Change the **budget** number beside the effect name to set the system's particle budget.
8. Press **Save**. It is enabled only while the effect has unsaved edits.

| Action | Control / key | Agent method |
|---|---|---|
| Open an effect | the path box, **Open** | `editor.vfx_open` |
| Read the open effect | the whole panel | `editor.vfx_state` (`sim_steps` runs the CPU reference simulation and returns its hash; `cost_time` returns the cost rows) |
| Add, remove, enable or disable a module; change a parameter | module rows, **Add module**, **Remove**, the A/B box | `editor.vfx_edit` (`add_module`, `remove_module`, `enable_module`, `disable_module`, `set_param`) |
| Add, move, delete a curve or gradient key | the curve and gradient widgets | `editor.vfx_edit` (`curve_add_key`, `curve_move_key`, `curve_delete_key`, `gradient_*`) |
| Add, remove, reorder an emitter; set the budget | **Add emitter**, **Remove**, **Up**, **Down**, **budget** | `editor.vfx_edit` (`add_emitter`, `remove_emitter`, `move_emitter`, `set_budget`) |
| Save | **Save** | `editor.vfx_edit` with `op: "save"` |
| Undo and redo | **Undo**, **Redo** on the tab; Ctrl+Z, Ctrl+Shift+Z | `editor.vfx_edit` (`undo`, `redo`), `editor.undo`, `editor.redo` |
| Render the preview | the **Preview** image | `editor.vfx_preview` (works without a window; `png: true` writes the frame) |

## Keyboard

The tab handles no keys of its own. Ctrl+Z and Ctrl+Shift+Z work through the shared undo stack
([Editor guide](index.md#keys-that-work-everywhere)). Buttons, boxes and number fields take focus
with Tab. The curve widgets and the gradient bar are drawn shapes that respond to the mouse only;
the selected gradient key also has a time field and a color field you can type into. An agent edits
any key with `editor.vfx_edit` ([Editor accessibility](accessibility.md)).

## Known limits

- The preview needs a GPU device; without one the panel says so (`editor.vfx_preview` renders
  headless and reports `vfx_editor_no_gpu` when no device exists).
- A curve or an emitter list holds at most 64 keys and 64 emitters.
- The module order inside an emitter is fixed by the file format; only emitters can be reordered.
- The `events` module cannot be added from the list: it needs a child emitter, so write the link in
  the file.
- A module disabled with the A/B switch is not written by **Save**.
- An unsaved effect is covered by the editor's document autosave and offered for recovery after a
  crash.

See also: [Editor surface](../../agents/editor.md), [Editor guide](index.md).
