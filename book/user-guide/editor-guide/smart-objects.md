# Smart objects

<!-- index: Smart object -->
<!-- index: Smart object definition -->

Panel ids: `smart_objects` and `smart_object_def`. Two tabs, two jobs. **Smart Objects** (`smart_objects`) lists the smart objects and slots in the world the editor is showing, with claim state, and can switch one off or take a claim back. **Smart Object Definition** (`smart_object_def`) edits a `.smartobject.ron` definition. State trees stay on [State trees](state-trees.md); that page does not document these tabs. Depth: [Smart objects (agents)](../../agents/smart-objects.md), [Smart-object definition editor](../../agents/smart-object-editor.md), [Gameplay panels](../../agents/editor-gameplay-panels.md). Spec: `docs/specs/39-smart-objects-v2.md`.

**Smart Objects** is in the default layout's bottom group. **View ▸ Reset layout** brings that tab back. **Smart Object Definition** is not in that dock. `editor.layout_apply {"open": "smart_object_def"}` adds it. **Open definition** on a selected object loads that object's file into the definition editor.

## Watch claims

1. Open **Smart Objects**. Each object lists its slots. A slot's state is `free`, `claimed`, `in_use` or `disabled`, with the holder's name, the claim priority and why the slot is off.
2. Select an object or a slot. The viewport draws a marker for each slot (a small cross, larger when selected) and a line to the holder while markers are on.
3. Switch the selected object or slot off, or back on. A second off-reason needs a second enable.
4. **Release** takes a slot's claim back from its holder.
5. The recent claim messages sit under the list. They need the smart-object systems in the world you are watching. The edit world does not run those systems, so it still lists authored objects and it may show no messages until you press Alt+P.

While play runs, a change is applied on the play world on the next frame. Read the outcome from the next panel refresh.

## Edit a definition

1. **Content ▸ New ▸ Smart object** writes a definition with one slot, `slot_0`, at the origin (`editor.create_asset` with `kind: "smart_object"`).
2. Open **Smart Object Definition**, type the project path and press **Open**.
3. Edit activity tags, how those tags combine with a slot (`union` or `slot_first`), the requester rule (`definition_and_slot`, `slot_only` or `slot_else_definition`), the requester query, conditions and behaviors. Then each slot: name, start enabled, position and yaw, pitch and roll, tags, and its entrances.
4. The line under the header is the runtime validator: **valid**, or the message (a bad slot name, too many slots, a non-finite offset, a query that is too deep). The forms still accept the edit. **Save** refuses an invalid definition.
5. With **Slot handles** on, select the preview entity (or a level object whose definition is this file). Drag a slot or an entrance in the viewport. Each drag is one undo step. The handles replace the transform gizmo on that object.

Every form edit is one undo step (Ctrl+Z). A definition you have not edited is never rewritten. **Save** writes the canonical file. A file that still has comments warns you: the first save drops them.

## Actions

| Action | Control | Agent method |
|---|---|---|
| Read objects, slots and messages | **Smart Objects** | `editor.smart_object_panel_get` |
| Select, show markers | The list and the marker switch | `editor.smart_object_panel_set` |
| Enable, disable, release | The row actions | `editor.smart_object_panel_control` |
| Read or edit a definition | **Smart Object Definition** | `editor.smart_object_def_state`, `editor.smart_object_def_edit` |

## Keyboard

Both tabs' buttons and fields are focusable. Ctrl+S with the pointer over **Smart Object Definition** saves, and refuses when the definition is invalid.

## Known limits

- Slot rows in one snapshot stop at 2048. The total count still includes the rest. Messages kept on screen stop at 32.
- Claim messages need `SmartObjectPlugin` in the world you are reading.
- Entity ids do not survive the switch between the edit world and the play world. The selection clears when play starts or stops.
- Definition files are format version 1. This tab does not change that format.

## See also

[State trees](state-trees.md), [Viewport](viewport.md), [Play-in-editor](play-in-editor.md), [Editor guide](index.md).
