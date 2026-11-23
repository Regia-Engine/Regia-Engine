# Shadow stability (sun shadows that hold still)

A sun shadow should not crawl when the camera turns. Each sun cascade is an orthographic sun
view fitted to a sphere around a slice of the camera frustum. The sphere's size depends only on
the slice's distances, the field of view and the aspect, so turning the camera keeps it. Its
center moves with the camera.

Every preset keeps that center on the cascade's own texel grid in light space
(`FrameQuality::stable_sun_cascades`, ADR-1236). All placements then share one world-fixed texel
lattice, so a static pole, rail or leaf covers the same shadow texels in every frame. Without the
snap, the lattice slides by part of a texel each frame, and thin shadows widen, thin out and
break up as the camera turns: shimmer.

Before ADR-1236 only paged sun shadows (Desktop High and Ultra) snapped. Mobile Low, Mobile High,
Desktop Low, Deck and the editor viewport shimmered. The lab baseline (`FrameQuality::LAB`) still
uses the free-sliding fit, because its goldens were blessed with it.

## Check it

`render.shadow_stability` places the active profile's cascades on the CPU, the same way the
renderer does, for every frame of a camera sweep. No frame needs to run.

```json
{ "method": "render.shadow_stability",
  "params": { "eye": [0, 1.7, 0], "target": [0, 1.45, -1], "sweep": "yaw", "amount": 30, "frames": 60 } }
```

All parameters are optional:

| Parameter | Default | Meaning |
|---|---|---|
| `eye`, `target` | `[0, 1.7, 0]`, 1 m ahead and 0.25 m down along −Z | The camera at the start of the sweep. |
| `sun` | `[-0.45, -0.72, -0.53]` | The sun's travel direction. |
| `sweep` | `"yaw"` | `yaw` (turn about the vertical), `pitch` (tilt) or `strafe` (move sideways). |
| `amount` | 30° (5 m for `strafe`) | The total turn in degrees, or the total move in meters. |
| `frames` | 60 | The number of frames along the sweep (2–1000). |
| `fov_y_deg`, `aspect`, `near`, `far` | 60, 16/9, 0.1, 2000 | The camera's lens. |
| `stable` | the profile's | What-if only: overrides `stable_sun_cascades` for this report. The renderer is not changed. |

Vector coordinates must remain finite as `f32`. The camera's target-minus-eye and the sun direction
must also have finite, nonzero squared lengths. Invalid vectors answer `F0002` before the sweep.

It returns, for example (the numbers are illustrative):

<!-- not-a-request -->
```json
{ "preset": "DesktopLow", "stable": true, "paged": false, "snap_texels": 2048, "resolution": 2048,
  "sweep": "yaw", "amount": 30.0, "frames": 60, "still": true, "still_threshold_texels": 0.02,
  "cascades": [ { "cascade": 0, "texel_m": 0.07, "max_drift_texels": 0.0004,
                  "mean_drift_texels": 0.0001, "moves": 59, "pairs": 59 } ] }
```

- **Drift** is how far a fixed world point slides inside its shadow texel from one frame to the
  next, in texels. 0 means static shadows stay put.
- **Moves** counts the frames in which the cascade moved. A stable cascade still moves, but only
  in whole texels, which a viewer cannot see.
- **Still** is true when every cascade stays under `still_threshold_texels` (0.02). The leftover
  is float rounding of the matrices: about a hundredth of a texel on Ultra's 1 cm cascade-0 texels.

## Compare before and after

1. Run `render.shadow_stability { "sweep": "yaw", "amount": 12, "frames": 48 }`. `still` should
   be `true`.
2. Run it again with `"stable": false`. Every cascade now drifts by tenths of a texel per frame,
   and `still` is `false`. That is the shimmer the snap removes.
3. To see it on screen, put the camera over thin casters (a fence, poles, scatter). Turn it slowly
   with a few `play.step` calls and take a `seeing.screenshot` after each one. On a stable profile
   the shadows do not change between shots.

**What is a bug.** `still: false` on any preset without `"stable": false` is a bug. So is a
`texel_m` that changes during a `yaw` or `pitch` sweep (the cascade radius must not depend on
where the camera looks). Report the parameters and the response.

## Limits

- The report covers the sun cascades only. The near sun tile (ADR-164) is always snapped to its own grid.
- Paged sun shadows (Desktop High and Ultra) may keep an older placement on the same lattice
  while it still covers the view. The report shows the fresh placement of each frame, which has the
  same drift.
- World coordinates far from the origin round more coarsely. At about 2 km out, rounding reaches
  about a hundredth of a 1 cm texel.
