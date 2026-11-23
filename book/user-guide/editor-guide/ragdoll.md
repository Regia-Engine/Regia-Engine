# Ragdoll

The **Ragdoll** tab edits a character's physics asset, a `.ragdoll.ron` profile: one body per
simulated joint (a capsule, sphere or box with a radius, mass and length) and the swing-twist limits
of each joint to its nearest simulated ancestor. Select a character in the level and the tab checks
joint names against its skeleton, can generate a humanoid profile, draws each body in the viewport,
and runs a **Drop test**. Depth: [Editor data panels](../../agents/editor-data-panels.md) (the
Ragdoll section) and [Active ragdolls](../../agents/ragdoll-active.md).

## Open it

The tab is in the default layout's bottom group; **View ▸ Reset layout** brings back a closed one.
An agent brings it forward with `editor.layout_apply {"open": "ragdoll"}`. There is no palette
command for this tab.

## Author a ragdoll

1. Type a project-relative path such as `assets/chars/hero.ragdoll.ron` in the path field and press
   **Open**, or press **New** to start an empty profile at that path.
2. Select the character in the level (an entity with a skinned mesh) and press **Use selected rig**.
   The line above the table names the rig; **Gizmo** shows or hides the bodies in the viewport.
3. Press **Humanoid from rig** to generate bodies and limits from the skeleton, or add a body by
   typing a joint name and pressing **Add body**.
4. In the table, edit each body's **radius**, **mass** and **length** (auto means it is measured from
   the skeleton), turn **limits** on or off, or press **Remove**. Select a row to choose its shape
   (capsule, sphere or box, with width and depth for a box), to tick **length from skeleton**, and
   to set its limits in degrees (**flexes backward (knee)** for a hinge that bends the other way).
5. Amber lines under the table are findings, such as a joint the skeleton does not have.
6. Press **Drop test**: the ragdoll falls limp onto a floor in a private physics world and the tab
   reports whether it settled, its fastest speed and its lowest body center; the final pose is drawn
   beside the rig. The level is not touched. The line says `(stale)` once the profile changes.
7. Press **Save**. It is enabled while there are unsaved changes and writes the file atomically.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| Open or start a profile | Path field, **Open**, **New** | `editor.ragdoll_panel_open` (`new: true` to start) |
| Read the profile, findings and preview | The tab | `editor.ragdoll_panel_get` |
| Edit bodies, shapes, limits, the forward axis | The table and row editors, **Add body**, **Humanoid from rig** | `editor.ragdoll_panel_edit` |
| Pick the rig, show the gizmo | **Use selected rig**, **Gizmo** | `editor.ragdoll_panel_set_rig` |
| Drop test | **Drop test** | `editor.ragdoll_panel_preview` |
| Save | **Save** | `editor.ragdoll_panel_save` |
| Undo, redo | **Undo**, **Redo** | `editor.ragdoll_panel_edit` with `op: undo` or `redo` |

## Keyboard

The tab handles no keys of its own; its controls are reached with Tab and pressed with Enter or
Space.

## Known limits

- Edits stay in memory until **Save**; **Undo** and **Redo** keep the last 32 steps.
- **Drop test** needs a rig and an open profile; without a rig the button is disabled.
- A profile that names a joint the rig lacks is flagged; if no body names a joint of the rig, the
  drop test answers `ragdoll_preview_no_bodies`.

See also: [Animation editor](anim-graph.md), [Editor guide](index.md).
