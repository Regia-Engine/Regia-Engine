# Animation robustness: clip reports and playback rules

`anim.clip_report` checks the clips a rig plays before they show up as a wrong pose: broken
key data, notifies that can never fire, root motion that drifts per lap, and how far the
cook's compression moves the joints. This page also lists the playback rules the evaluator
follows (ADR-871), so an agent knows what a graph should do and can verify it with
`anim.get_state`, `play.step` and screenshots.

## Report a rig's clips

```json
{ "entity": 4294967310 }
→ { "entity": 4294967310, "skeleton": 52,
    "tolerances": { "position": 0.005, "rotation": 0.005, "scale": 0.005, "propagated": 0.01 },
    "issue_count": 1,
    "clips": [
      { "name": "walk", "duration": 1.0, "tracks": 52, "notifies": 2,
        "root_motion": { "mode": "XZ", "joint": 0, "per_lap": [0.0, 0.0, -1.38], "yaw_per_lap": 0.0 },
        "loop_seam": { "max_rotation": 0.004, "max_translation": 0.0, "joint": 12, "joint_name": "spine_02" },
        "issues": [],
        "compression": { "max_position": 0.0041, "max_rotation": 0.0047, "max_scale": 0.0,
                         "max_object_position": 0.012, "worst_joint": 31, "worst_joint_name": "hand_l",
                         "worst_time": 0.48, "samples": 121, "within_tolerance": true,
                         "raw_bytes": 61224, "compressed_bytes": 9310, "ratio": 0.152 } },
      { "name": "wave", ...,
        "issues": [ { "code": "notify_outside_clip",
                      "detail": "notify \"wave_end\" at 2.4 s is outside 0..=2 s" } ], ... } ] }
```

Parameters (all optional except `entity`):

| Param | Meaning |
|---|---|
| `clip` | Report only this clip. An unknown name is `NOT_FOUND`. |
| `compression` | `false` skips the compression pass (cheaper on large libraries). |
| `position_tolerance`, `rotation_tolerance`, `scale_tolerance`, `propagated_tolerance` | Cook tolerances to measure against (meters, radians, scale, meters). Each must be a finite number from the smallest positive `f32` through 1000, else `INVALID_PARAMS`. Zero is refused. |

Clips are sorted by name. `skeleton` is the rig's joint count (`null` without a
`SkinnedMesh`; then the compression error is local only and `max_object_position` is 0).

### Loop seam

`loop_seam` says how far each clip's last frame is from its first, per joint in local space:
`max_rotation` (radians), `max_translation` (meters) and the joint with the largest seam. A
looping clip pops by that much at every loop point, so a clean loop reads close to 0. The
root-motion axes the clip extracts are left out, because the character controller carries them
across the loop. A walk without root-motion extraction shows its whole stride as a translation
seam: the rig snaps back every loop.

### Issue codes

Every `issues[].code` is one of these stable strings:

| Code | Meaning | What plays wrong |
|---|---|---|
| `bad_duration` | The clip duration is not a finite positive number. | The clip is stuck at time 0. |
| `non_finite_key` | A key time or value is NaN or infinite. | A sample is NaN when the segment it interpolates, or the endpoint it clamps to, contains that key. |
| `unsorted_times` | Key times do not strictly increase. | Keys play out of order. |
| `key_count_mismatch` | A curve has more times than values (or the reverse). | An unequal joint channel is skipped, so the joint keeps the pose's current value (the bind pose, when the evaluator just copied it). A morph track or a named curve with the same mismatch plays the paired prefix. |
| `key_outside_clip` | A key lies before 0 or after the clip end. | The outside key is still an interpolation endpoint. Playback clamps the clock to the clip; sampling clamps the clock to the key range. Neither drops the key. |
| `non_unit_rotation` | A rotation key is more than 1 % off unit length. | Skinning scales the mesh. |
| `notify_outside_clip` | A notify lies outside `0..=duration`. | It never fires. |
| `joint_out_of_range` | A track animates a joint the rig's skeleton does not have. | The track is skipped. |
| `duplicate_track` | Two tracks animate the same joint. | The later sampleable channel wins. A later channel that is not sampleable is skipped, so the earlier value remains. |
| `root_motion_track_missing` | Root motion names a joint the clip has no track for. | No root motion is extracted. |

### Compression error

`compression` cooks the clip with the given tolerances and samples the cooked clip against the
source at every key time of either and every midpoint between them. `max_position`,
`max_rotation` and `max_scale` are local joint errors; `max_object_position` is the
object-space joint error with every ancestor's error added up (what a viewer sees on a hand
or a foot). `within_tolerance` says whether the local errors respect the tolerances.

Key reduction quantises each key first and measures the reconstructed chord against the full
tolerance, so `within_tolerance` holds for ordinary clips. It is `false` when a track spans a
range too large for 16-bit keys at that tolerance: half a step of a 2 km root track is about
15 mm, more than 5 mm. Split such a clip, or cook it with a looser `position_tolerance` on
purpose.

## Playback rules the evaluator follows

These hold for every graph (ADR-871):

- **Root motion blends like the pose.** A blend space at 50/50 between walk (1 m/s) and run
  (3 m/s) moves the character at 2 m/s. A cross-fade weights the source by `1 − alpha` and the
  target by `alpha`; a clip without root motion counts as standing still. A masked layer
  contributes root motion only where its mask covers the skeleton root.
- **Long frames count every lap.** A frame that covers several loops of a clip (a hitch, a far
  LOD animator updating a few times a second) adds each lap's root motion and fires each
  notify once per lap (at most 16 times per notify per frame).
- **Reverse playback** (`speed < 0`, or a negative animator speed) plays from the end, wraps at
  the start when looping, moves root motion backwards and fires notifies in reverse order. A
  one-shot played in reverse finishes at its start (`ClipFinished`).
- **A notify at time 0 fires** on the first frame of a clip and on every loop.
- **A clip under half weight is silent.** Notifies fire only when the clip's effective weight is at
  least one half (`0.5 - 1e-4`). A 50/50 blend fires both sides. A blend at alpha 0.4 (parent weight 1)
  fires only the heavy side. A synced blend space gives each sample `weight × sample weight`, so a
  dominant sample under one half is silent too, even when it is the loudest sample. Entering a state
  uses the machine's audible weight: an enter under one half fires no start notify. A clip at weight 0
  (the far input of a blend pinned to one side, a cross-fade source on its last frame) is the same
  rule. `Notify` transition conditions still see every notify name, so a machine in a hidden branch
  keeps following them.
- **Entering a state restarts it.** The second jump plays the jump clip from the start again.
  An any-state transition (`from: None`) never re-enters the state that is already current, so
  a condition that stays true (`IsDead`) enters once instead of restarting every frame.
  An explicit self-transition (`from: Some("Hit"), to: "Hit"`) restarts the state.
- **Synced blend spaces** advance on one clock at the weighted average of the active samples'
  durations, so the cadence changes smoothly with the parameter, and keep that phase when the
  parameter jumps (sprint from a walk keeps the same foot forward). A sample fading in starts at
  the shared phase (no root-motion spike). Only the dominant sample fires notifies, and only when
  its effective weight is at least one half, so footsteps are not doubled and a dominant sample
  under half is silent.
- **Morph weights blend like the pose.** A target one side of a blend lacks counts as 0, so a smile
  fades out over a cross-fade instead of popping at its end. A face layer at weight 0.5 shows half
  the expression. An additive clip adds its morphs to the base.
- **Named curves** sampled by several clips are blended by the same weights.
- **Bad numbers do not reach the pose.** A NaN or infinite blend weight reads as 0, a NaN
  blend-space input as 0, and an IK node whose target is non-finite, or a socket that has not
  been placed yet, leaves the pose alone. Graph files with NaN or infinite speeds, sample
  positions, look-at limits or layer easing times, or a negative transition duration, fail to
  load with a message naming the field.
- **IK settings never panic.** Look-at limits and pelvis ranges are magnitudes (their sign is
  ignored, NaN means no limit for look-at and 0 for pelvis ranges); joint limits may come in
  either order; very short bones are solved.

## Verifying a fix

1. `anim.clip_report { entity }`: no `issues`, `within_tolerance: true`.
2. `anim.set_param` to the blend you want to see, `play.step`, and read `root_motion.delta`
   from `anim.get_state`: it changes smoothly as you sweep the parameter.
3. `seeing.screenshot` to judge the pose by eye.
