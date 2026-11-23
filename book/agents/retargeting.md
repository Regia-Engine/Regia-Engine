# Retargeting editor (`editor.retarget_*`)

Spec: `docs/specs/85-retarget-crowds.md` § 2.5 (ADR-7801). The Retarget panel (a tab of the bottom group of the default layout,
panel id `retarget`) maps one rig's bones onto another's by body chain, aligns chains to a reference pose, previews a source
clip on both rigs side by side and bakes clips for the target. The panel and these methods edit the same state.

The map is saved as an ordinary `.retarget.ron` setup (format v2, `docs/formats/retarget-ron.md`): both sides as
`Explicit` role → joint maps, each chain's alignment as the `rest_offset_deg` of every role in the chain, and the IK
goals list. The
preview and the batch use the runtime `Retargeter` built from that setup, so a game that loads the file plays what
the panel shows.

To check a retarget on placed rig entities instead, use `anim.retarget_check`
([Retargeting on real content and crowds](retarget-crowds.md)).

## Open two rigs

A rig is a built-in name or a project file:

- `demo`: the editor's procedural biped (six clips);
- `fixture:source`: a Mixamo-style rig in an A-pose, facing −Z, with a tail and four clips (`Wave`, `Stride`,
  `Nod`, `Turn`);
- `fixture:target`: a game-style rig in a T-pose, facing +Z, with an upper-arm twist joint and a tail;
- a `.glb`/`.gltf` (its first skin and its clips) or a `.bvh` (default import settings; its one clip).

```json
{"method": "editor.retarget_open",
 "params": {"source": "fixture:source", "target": "fixture:target"}}
→ { "open": true,
    "source": { "name": "fixture:source", "joints": 24, "clips": ["Wave", "Stride", "Nod", "Turn"] },
    "target": { "name": "fixture:target", "joints": 26 },
    "chains": [ { "chain": "root", "label": "Root", "mapped": 0, "roles": 1,
                  "rest_offset_deg": [0, 0, 0], "rows": [] }, ... ],
    "other": [], "path": null, "dirty": false, "clip": "Wave", "batch_job": null }
```

The map starts empty. Pass `setup` (a `.retarget.ron` path) to open with a saved map: each side's role source
(`Guess`, `ByName`, `Profile`, `Explicit`, plus overrides) is resolved against its skeleton into an explicit map.

## Auto-map

```json
{"method": "editor.retarget_auto_map", "params": {}}
→ { ...state..., "by_chain": ["chest", "head", "hips", ...], "by_name": ["tail_1", "tail_2"] }
```

The rules, in order:

1. **By chain.** Both skeletons' humanoid roles are guessed from joint names (the rules in
   [Retargeting on real content and crowds](retarget-crowds.md#roles)). Every role both rigs have is paired. The
   roles group into seven chains:

   | Chain | Roles |
   |---|---|
   | `root` | `hips` |
   | `spine` | `spine`, `chest`, `upperChest` |
   | `head` | `neck`, `head` |
   | `left_arm`, `right_arm` | `leftShoulder`, `leftUpperArm`, `leftLowerArm`, `leftHand` (and right) |
   | `left_leg`, `right_leg` | `leftUpperLeg`, `leftLowerLeg`, `leftFoot`, `leftToes` (and right) |

2. **By name.** Each target joint still unmapped pairs with the first unmapped source joint whose name matches
   after dropping a `namespace:` prefix, case and every character that is not a letter or a digit
   (`mixamorig:Tail1` ↔ `tail_1`). The pair becomes a custom role named after the target joint, listed under
   "Other".

Twist, IK, end and helper joints never take a humanoid role. Auto-map replaces the map and keeps the rest offsets
of roles that are still mapped.

## Edit the map

```json
{"method": "editor.retarget_map_set", "params": {"role": "leftHand", "target": "hand_l"}}
{"method": "editor.retarget_map_set", "params": {"role": "tail_2", "source": null}}
{"method": "editor.retarget_map_set", "params": {"chain": "left_arm", "rest_offset_deg": [0, 0, -10]}}
{"method": "editor.retarget_map_set", "params": {"chain": "left_arm", "align": "t_pose"}}
→ { ...state..., "changed": ["align"] }
```

- `role` with `source` and/or `target` sets that side's joint, or clears it with `null`. A role may be any name;
  humanoid role names list under their chain, others under "Other".
- `chain` with `rest_offset_deg` sets the chain's rest-pose alignment: XYZ Euler degrees, object space, written to
  every role of the chain.
- `chain` with `align` (`t_pose` or `a_pose`) computes that offset. With it, the target's aim bone points along the
  reference direction while the source stands in its bind pose:
  - arms out to the target's sides (level for a T-pose, 45° down for an A-pose);
  - legs straight down;
  - spine and head straight up.

  The aim bones are upper arm → forearm, thigh → shin, spine → chest and neck → head. The root has none.

Without an offset, the runtime's own rest correction already turns each target bone onto the source's bind
direction, so a T-pose target driven by an A-pose source shows the A-pose at rest. Align a chain when the target
should show the reference pose instead.

## Preview

```json
{"method": "editor.retarget_preview", "params": {"clip": "Stride", "time": 0.3}}
→ { "clip": "Stride", "time": 0.3, "duration": 1.0, "playing": false,
    "facing_yaw_deg": 180.0, "translation_scale": 0.9, "mapped_joints": 24,
    "source_root": [x, y, z], "target_root": [x, y, z],
    "chains": [ { "chain": "spine", "source_dir": [0, 1, 0], "target_dir": [0, 1, 0], "angle_deg": 0.0 }, ... ] }
```

The source clip is sampled on the source rig at `time` and retargeted with the current map. For each chain the
result gives the aim bone direction on both rigs (the source's turned by the facing yaw) and the angle between
them: near 0 for a chain without an offset, the offset's angle for an aligned chain. `playing: true` plays the
panel's preview; the panel draws both rigs side by side, the source turned to face the same way as the target.

## Save

```json
{"method": "editor.retarget_save", "params": {"path": "assets/anim/mixamo-to-guard.retarget.ron"}}
→ { "path": "...", "text": "(\n    format_version: 2, ..." }
```

The file is written atomically. Without `path` the map goes back to the file it was opened from or last saved to.

## Retarget clips (batch)

```json
{"method": "editor.retarget_batch",
 "params": {"out_dir": "cooked/retargeted", "clips": ["Wave", "Stride"], "wait": true}}
→ { "job": "retarget_batch:1", "done": true,
    "result": { "written": [".../Wave.cooked", ".../Stride.cooked"], "count": 2, "cancelled": false } }
```

Each selected source clip (all of them when `clips` is absent) is baked with `Retargeter::retarget_clip` and
written as `<out_dir>/<clip>.cooked`. That file is a cooked-file container holding the compressed
`regia_animation::AnimationClip` payload, the same encoding the cook writes for a retargeted BVH clip
(`retarget/<name>`). Characters other than letters, digits, `-`, `_` and `.` in a clip name become `_`.

The batch is an editor job (`retarget_batch:<n>`):

- `agent.job_status` and the background-task row show its progress.
- `editor.cancel_job { id }`, `agent.job_cancel` and the panel's Cancel button stop it.
- Clips are staged in a hidden folder and moved into `out_dir` only after every clip is baked. A cancelled
  batch writes no clip and leaves no partial file; a move that fails part way removes the clips it had
  already moved, so a failed batch leaves none either.
- Without `wait`, the call returns at once with the job id.

A baked clip's end pose matches the runtime `Retargeter`'s pose for the same map within the cook's compression
tolerances (`CompressionSettings::default()`: 0.005 rad, 5 mm). The test
`a_baked_clips_end_pose_matches_the_runtime_retargeter_for_the_same_map` prints the measured worst error.

## Errors

Errors are `invalid_params` with `data.reason`:

| Reason | When |
|---|---|
| `retarget_not_open` | No source and target rig are open. |
| `retarget_bad_rig` | `source`/`target` is missing, not a built-in, or does not load (not `.glb`/`.gltf`/`.bvh`, no skin). |
| `retarget_unknown_role` | An empty role name. |
| `retarget_unknown_joint` | A joint the rig on that side does not have, or a value that is not a name or `null`. |
| `retarget_unknown_chain` | A chain other than `root`, `spine`, `head`, `left_arm`, `right_arm`, `left_leg`, `right_leg`. |
| `retarget_bad_offset` | `rest_offset_deg` is not three finite numbers. |
| `retarget_bad_pose` | `align` is not `t_pose` or `a_pose`. |
| `retarget_cannot_align` | The root, or a chain whose aim bones are not mapped on both rigs (an arm also needs the target's left and right legs or arms). |
| `retarget_nothing_to_set` | `editor.retarget_map_set` got neither a role edit nor a chain edit. |
| `retarget_build_failed` | The map does not build a retargeter (no shared role, or a joint that is gone). |
| `retarget_unknown_clip` | The source rig has no such clip, or no clips at all. |
| `retarget_io` | A file cannot be read or written, or `path`/`out_dir` is missing. |
| `retarget_busy` | A batch is already running. |
| `job_start_failed` | The batch's worker thread could not start. |
| `bad_setup` | A `setup` file does not parse, has another `format_version`, or names an unknown built-in profile. |

Paths follow the editor's document-path rule (project-relative, or absolute under the project or the temp
directory): `path_outside_project` otherwise. The panel's rig paths, Save path and output folder follow the
same rule and show the refusal in the panel's status line.

## Limits

- Writing the map into a BVH file's import settings is not built yet. The cook retargets a BVH clip through
  `BvhRetarget.bone_map` (source joint → target joint), and that field cannot carry humanoid roles (hip-height
  scaling, facing) or rest offsets. A `setup` field naming a `.retarget.ron` is listed for the next format step.
  Until then, bake clips with the batch, or load the `.retarget.ron` at runtime with
  `RetargetSetup::from_ron_str` and `RetargetSetup::build`.
- Crowds are out of scope for the panel (see spec 85 § 3).

## IK goals

A retarget keeps the target's bone lengths and scales only the hips. On a target whose legs or arms have other
proportions, planted feet then slide or sink and two hands on a prop drift apart. IK goals fix this at bake
(ADR-8476). Each goal is an `[upper, lower, end]` chain of roles mapped on both rigs. After the pose transfer, the
two-bone solver puts the chain's end where the source put it, times the translation scale:

- `Contacts` (feet): the goal holds still while the source end is planted, at the target's own ankle height.
- `ScaledWorld` (hands on a prop, a weapon grip): the goal follows the source on every frame.

The goals are saved in the map's `ik_goals` list. The batch bakes them, and `anim.retarget_check` reports them
([IK goals in the check](retarget-crowds.md#ik-goals)).

```json
{"method": "editor.retarget_goal_set", "params": {"preset": "feet", "preview": true}}
{"method": "editor.retarget_goal_set",
 "params": {"roles": ["rightUpperArm", "rightLowerArm", "rightHand"], "mode": "ScaledWorld", "weight": 1.0}}
{"method": "editor.retarget_goal_set", "params": {"index": 2, "mode": "Contacts"}}
{"method": "editor.retarget_goal_set", "params": {"index": 2, "remove": true}}
→ { ...state..., "ik_goals": [ { "roles": ["leftUpperLeg", "leftLowerLeg", "leftFoot"], "mode": "Contacts",
                                 "weight": 1.0 }, ... ],
    "goal_preview": true, "changed": ["preset", "preview"],
    "checks": [ { "roles": [...], "mode": "Contacts", "weight": 1.0, "resolved": true, "max_residual": 0.0001,
                  "mean_residual": 0.00003, "planted_frames": 16, "planted_slide": 0.0002 }, ... ] }
```

- `preset` appends `feet` (`Contacts` goals on both legs) or `hands` (`ScaledWorld` goals on both arms).
- `roles`, `mode` and `weight` add a goal, or change goal `index` when it is given. A new goal needs `roles`; its mode
  defaults to `ScaledWorld` and its weight to 1.
- `remove: true` removes goal `index`.
- `preview` sets the panel's "Preview with goals" toggle. With it on, `editor.retarget_preview` (which also takes
  `goals`) shows the bake with the goals solved, sampled at the preview time. With it off, it shows the plain pose
  transfer.
- `checks` has, per goal, the residual and planted slide on the preview clip (empty without a clip). The fields are
  those of [`clip.goals` in the check](retarget-crowds.md#ik-goals).

In the panel, the goals list sits under the preview: role pickers for the three joints, the mode, the weight, a
Remove button, and below each goal its residual and planted slide.

### Goal errors

| Code | Meaning |
| --- | --- |
| `retarget_bad_goal` | `roles` is not three distinct, non-empty role names, `mode` is not `ScaledWorld` or `Contacts`, `weight` is outside `0..=1`, or `preset` is not `feet` or `hands`. |
| `retarget_unknown_goal` | There is no goal at `index`, or `remove` has no `index`. |
| `retarget_not_open` | No rigs are open. |
