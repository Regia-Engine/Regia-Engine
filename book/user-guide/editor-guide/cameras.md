# Cameras

<!-- index: Camera rig -->
<!-- index: Cameras panel -->

Panel id: `cameras`. The **Cameras** tab is the asset page for every `.camera.ron` in the project, the rig graph of the asset you select, and the camera drivers of a running game. Depth: [Camera framework](../../agents/camera-framework.md), [Gameplay panels](../../agents/editor-gameplay-panels.md).

The tab is in the default layout's bottom group. The Cinematics workspace opens it with shots and video. **View ▸ Reset layout** restores a closed tab. An agent opens it with `editor.layout_apply {"open": "cameras"}`. **Content ▸ New ▸ Camera rig** creates a file (`editor.create_asset` with `kind: "camera"`): one `look` slot and a rig named `follow`.

The page holds the edit world's assets. No driver runs there, so editing does not move a camera in a running game. **Save** writes the file; the asset watcher reloads it in the edit world and in the play world.

## Edit a rig

1. Open the tab. The first time it draws, it scans the project. Pick an asset. The **Asset** section lists status, issues, parameters, variables, input slots and the director.
2. Edit **Parameters**, **Rigs** (rename, duplicate, remove, add a node, tags, blend out) and **Transitions**. Each change is one step of that asset's own history. **Undo** and **Redo** walk it. A drag or a typing session in one field is one step until you let go.
3. Open **Rig graph**. Boxes are laid out from the asset. Click a node for its form (move, duplicate, remove, add after, add inside a group). Green boxes ran, red ones were skipped, gray ones are switched off. Arcs on the left are transitions.
4. **Apply** validates the working copy and, with no errors, makes it the active version. **Save** (Ctrl+S while the pointer is over the panel) applies, writes the canonical file and marks the history saved. **Save\*** means unsaved edits.
5. Press Alt+P. The **Live** section lists drivers: the asset they name, whether they are bound, the primary rig, blend, layers and variables. Pick one to inspect it.

## Actions

| Action | Control | Agent method |
|---|---|---|
| Read assets, the page and drivers | The tab | `editor.camera_panel_get` |
| Select an asset or a driver | The lists | `editor.camera_panel_select` |
| Load one file, or scan | Opening the tab, or a path | `editor.camera_panel_load` |
| Read the graph | **Rig graph** | `editor.camera_rig_graph` |
| Edit the working copy | The forms | `editor.camera_panel_edit` |
| Apply, save | **Apply**, **Save** | `editor.camera_panel_apply`, `editor.camera_panel_save` |
| Drive a running camera | **Live** | `editor.camera_panel_driver` |

## Keyboard

Tab moves between the forms. Ctrl+S saves while the pointer is over the panel. The graph is operated by click; the same edits are on the forms.

## Known limits

- A file that changes on disk replaces the working copy. Edits you have not saved are lost. **Apply** alone does not write the file.
- Saving writes the canonical text. Comments in the old file do not survive. The tab says so while the file still has comments.
- A `Hook` director cannot be fully checked in the edit world, which has no game code to ask. The play world checks the name.
- While play runs, a call that changes a driver is queued for the next frame. Read `last_call` on the next `editor.camera_panel_get`. Entity ids from the edit world and the play world are not the same; the selection clears when play starts or stops.
- `play_framework` stays false until the game adds `CameraPlugin`. A play session with no camera drivers says so.

## See also

[Play-in-editor](play-in-editor.md), [Sequencer](sequencer.md), [Editor guide](index.md).
