# BVH motion-capture import (`editor.bvh_inspect`)

Motion-capture libraries ship as Biovision hierarchy (`.bvh`) files. Regia cooks one to the animation formats it
already has (ADR-2532): the file's **skeleton** is the primary output (`regia_animation::Skeleton`) and its
**clip** is the sub-asset `clip/<name>` (`regia_animation::AnimationClip`). A BVH has no mesh, so nothing else is
written. Drop `hero_walk.bvh` under `assets/` and `regia cook` (or the editor import) does the rest. No cooked format
is added, so there is no new format version and no migrator.

## What is read

`HIERARCHY` with `ROOT`, `JOINT` and `End Site`, each with `OFFSET` and (except end sites) `CHANNELS`, then `MOTION`
with `Frames:`, `Frame Time:` and one line of values per frame. Channels are `Xposition`, `Yposition`, `Zposition`
(each at most once per joint) and up to three of `Xrotation`, `Yrotation`, `Zrotation` in **any order**.

- Rotations are degrees and compose in the order the file lists them, first channel outermost:
  `CHANNELS 3 Zrotation Xrotation Yrotation` is `Rz * Rx * Ry` (column vectors).
- A joint's local translation is its `OFFSET` plus its position channels.
- A joint with no channels stays at its `OFFSET`.
- `End Site`s are dropped unless `end_sites: true` (they become joints named `<parent>_end`).
- Files with several `ROOT`s keep all of them.

## Settings

The `.meta` `settings` string is RON; every field is optional.

| Field | Default | Meaning |
| --- | --- | --- |
| `unit` | `Meters` | Unit of the file's lengths: `Meters`, `Centimeters`, `Millimeters`, `Inches`, `Feet`. |
| `unit_scale` | `1.0` | Extra factor after `unit` (must be positive). |
| `up_axis` | `Y` | `Z` turns the roots -90 degrees about X so +Z becomes +Y. |
| `keep_forward` | `false` | `false` bakes the project's 180 degree yaw into the roots, as the glTF importer does (ADR-009). |
| `root_motion` | `None` | `XZ`, `XYZ` or `Yaw`: tags the first root as the clip's root-motion joint, like the glTF path. |
| `rotation_tolerance_degrees` | `0.05` | Key reduction error bound for rotations. `0` keeps every key. |
| `translation_tolerance` | `0.0005` | Key reduction error bound for translations, meters. `0` keeps every key. |
| `end_sites` | `false` | Keep `End Site`s as joints. |
| `clip_name` | file stem | Name of the clip. |
| `retarget` | none | See below. |
| `limits` | see below | Hard caps on untrusted input. |

Key reduction drops a key while the curve without it stays within the tolerance of the original keys
(Ramer-Douglas-Peucker over time, per curve); a curve that never leaves tolerance of its first key becomes a constant.

Limits (`limits`): `max_file_bytes` 256 MiB, `max_joints` 1,024, `max_frames` 1,000,000, `max_channels` 8,192 per
frame, `max_depth` 256.

Example, a library in centimeters, Z-up, with root motion:

```ron
(unit: Centimeters, up_axis: Z, root_motion: XZ, rotation_tolerance_degrees: 0.1)
```

## Playing it on a project character (`retarget`)

```ron
(
  unit: Centimeters,
  retarget: Some((
    target: "assets/characters/guard.glb",
    skin: 0,
    bone_map: {"Hips": "pelvis", "Spine": "spine_01"},
  )),
)
```

The cook reads the target's skin and also writes `retarget/<name>` (the clip retargeted by the existing retargeter,
`docs/specs/85-retarget-crowds.md`) with `retarget/skeleton/<first joint>`, a copy of the target skeleton the
retargeted clip depends on. `bone_map` is source (BVH) joint to target joint. Leave it empty and both skeletons'
humanoid roles are guessed from joint names, as the retarget setup does. The target file's bytes are a cook input, so
editing the character recooks the clip. Targets are `.glb`/`.gltf` files.

## Agent method: `editor.bvh_inspect`

Read-only. Give exactly one of `path`, `text` or `sample: "fixture"`, and optionally `settings` (the RON text). `path` is
relative to the project, or absolute under the project or the temp dir ([Request paths](safety.md#request-paths),
ADR-7388); any other path is `path_outside_project`.

```json
{"method": "editor.bvh_inspect", "params": {"path": "assets/hero_walk.bvh", "settings": "(unit: Centimeters)"}}
```

Reply: `{ clip, joints, roots, joint_names, duration_seconds, sample_rate, tracks, keys_before, keys_after,
root_motion, meters_per_unit, retarget }`. Try it without a file:

```json
{"method": "editor.bvh_inspect", "params": {"sample": "fixture", "settings": "(unit: Centimeters, root_motion: XZ)"}}
```

### Error codes

`data.bvh_error` holds one of these (lowercase; the cook reports the same text and code):

| Code | Cause |
| --- | --- |
| `bvh_bad_params` | Wrong combination of `path`/`text`/`sample`, `settings` is not a string, or inline `text` is over 16 MiB. |
| `bvh_io` | The file cannot be read. |
| `bvh_too_large` | The file is over `max_file_bytes`. |
| `bvh_syntax` | A malformed statement in `HIERARCHY` (the message carries the line number): bad `OFFSET`, unexpected token, unbalanced braces, missing `MOTION`, a non-finite number. |
| `bvh_channels` | A bad `CHANNELS` line: wrong count, unknown or repeated channel, more than three rotations. |
| `bvh_duplicate_joint` | Two joints share a name. |
| `bvh_motion` | `Frames:`/`Frame Time:` missing or invalid, or a frame line with the wrong number of values, or the wrong number of frame lines. |
| `bvh_limit` | Joints, frames, channels or nesting past `limits`. |
| `bvh_bad_settings` | The settings RON does not parse or a number is out of range. |
| `bvh_retarget` | The retarget target cannot be read, has no such skin, or no roles pair (cook only). |

## Fuzzing

`import.bvh` is in `fuzz.targets` in the editor (parse, convert, reduce keys, retarget onto itself).
