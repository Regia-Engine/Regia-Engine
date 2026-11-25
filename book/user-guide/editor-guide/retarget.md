# Retarget

Panel id: `retarget`.

The **Retarget** tab maps one rig's bones onto another's, so animations made for one character play on another. You
open a source and a target rig, map their bones chain by chain (automatically, then by hand where needed), line the
chains up with a reference pose, watch a clip play on both rigs side by side, save the map as a `.retarget.ron` file
and bake clips for the target. A game that loads the saved map plays what the tab shows. Depth: [Retargeting editor
(agents)](../../agents/retargeting.md), [Retargeting on real content and crowds](../../agents/retarget-crowds.md).

## Open it

The tab is in the default layout's bottom group. From the keyboard press Ctrl+Shift+P and pick **Open the Retarget
tab** (`window.open_retarget`). An agent brings it forward with `editor.layout_apply {"open": "retarget"}`.

## Open two rigs

Type a rig in **Source** and in **Target** and press **Open rigs**. A rig is `demo` (the editor's procedural biped),
`fixture:source` or `fixture:target` (two test rigs: an A-pose rig facing −Z and a T-pose rig facing +Z), or the path of
a `.glb`, `.gltf` or `.bvh` file in the project. The map starts empty.

## Map the bones

1. Press **Auto-map**. It pairs every humanoid role both rigs have (guessed from the joint names) and groups them into
   chains: Root, Spine, Head, the two arms and the two legs. Target joints still unmapped then pair with a source joint
   of the same name (ignoring a `namespace:` prefix, case and punctuation, so `mixamorig:Tail1` matches `tail_1`); those
   are listed under **Other**. The status line says how many it mapped each way.
2. **Bone map** lists each chain with its count of mapped roles; open one to see its rows. In a row, pick the
   **Source joint** and the **Target joint** for the role from their lists, or clear one.
3. Twist, IK, end and helper joints never take a humanoid role.

## Line up the rest pose

When the target should stand in a reference pose rather than copy the source's rest pose, set the chain's **Rest
offset** (X, Y and Z, in degrees), or press **Align to T-pose** or **Align to A-pose**: the editor computes the offset
that points the target's arm, leg, spine or neck bones the reference way while the source stands in its bind pose. Without an offset, the target follows the source's rest pose.

## Preview

Under **Preview**, pick a clip of the source rig, press **Play** (or **Pause**), or drag the time slider. Both rigs are
drawn side by side, the source turned to face the same way as the target.

## Save and bake

- Type a path ending in `.retarget.ron` and press **Save map**. "unsaved" next to the button means the map changed
  since the last save.
- To bake clips for the target, tick the source clips to bake, type an **output folder** and press **Retarget clips…**.
  Each clip is written as `<folder>/<clip>.cooked`. The bake runs as an editor job; **Cancel** stops it, and a cancelled
  or failed bake writes no clip at all.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| Open the rigs (and a saved map) | **Source**, **Target**, **Open rigs** | `editor.retarget_open {"source", "target", "setup"?}` |
| Map automatically | **Auto-map** | `editor.retarget_auto_map` |
| Set or clear a role's joint | **Source joint**, **Target joint** | `editor.retarget_map_set {"role", "source"?, "target"?}` |
| Set or compute a chain's rest offset | **Rest offset**, **Align to T-pose**, **Align to A-pose** | `editor.retarget_map_set {"chain", "rest_offset_deg" \| "align"}` |
| Preview a clip | **Preview** clip, **Play**, time slider | `editor.retarget_preview {"clip", "time", "playing"?}` |
| Save the map | **Save map** | `editor.retarget_save {"path"?}` |
| Bake clips | **Retarget clips…**, **Cancel** | `editor.retarget_batch {"out_dir", "clips"?, "wait"?}`, `editor.cancel_job` |

Tab and Shift+Tab move between the controls; Space or Enter presses the focused button.
