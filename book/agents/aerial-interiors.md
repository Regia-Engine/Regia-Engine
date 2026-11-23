# Aerial perspective inside interiors

The outdoor composite (the physical atmosphere's sky pass) adds aerial perspective to every opaque
pixel: the sky's in-scatter along the view ray, read from the aerial table by distance, and the
transmittance of that air. Outdoors that is the haze that makes far hills blue. Inside a bunker,
a tunnel or a cave the air is under a roof, so the sky does not light it, and the same term made
closed rooms read bluer than they are (ADR-4108 § 3(b): the v0.3.0 gallery plate read 4 % to 13 %
bluer than the path tracer where the sun never reaches).

Since ADR-4757 the composite honors `InteriorVolume`:

- the stretch of each view ray inside an interior box takes **no in-scatter**. The table holds the
  in-scatter integrated from the eye, so the stretch `[a, b]` adds `L(b) − L(a)`; the composite
  subtracts it (Hillaire 2020 § 5.4, single scattering along one ray);
- the **transmittance is unchanged**: the light that reaches the eye from beyond still crosses
  that air;
- **four** volumes are read, the same slots the froxel fog reads (`AERIAL_INTERIOR_SLOTS`): all of them when
  there are four or fewer, else the four usable boxes nearest the camera (`aerial_interior_slots`, ADR-7661), so
  the box the camera stands in is always read (one volume per storey, several buildings).
  Where boxes overlap the shared stretch is subtracted once. A fifth volume still shades surfaces
  (the interior shade) but leaves the air outdoor;
- there is **no pop** at a door or a cave mouth: the stretch shrinks with the eye's step and is
  zero at the box's face, so the in-scatter changes continuously as the camera walks out;
- a ray that crosses **no box** takes the old code path, so outdoor frames are bit-identical. A
  stretch shorter than 1 mm (`AERIAL_INTERIOR_MIN_SPAN_M`, a ray grazing a face) is ignored;
- all three aerial entries do this: `fs_aerial` (the default), `fs_aerial_tables` and the lite
  `fs_aerial_tables_lite` (`aerial_from_tables`, ADR-3294). The rain veil (`fs_weather_veil`) also
  skips the stretch: no rain falls inside an interior, so it neither dims nor scatters there.

Bit identity holds the camera, lighting and history inputs fixed. After a walk through a changed
interior, automatic exposure and temporal history may retain differences in later outdoor frames.

The rule follows the level's `InteriorVolume` boxes, like the froxel fog. It is on by default;
`FrameRenderer::set_aerial_interiors(false)` (agent: `render.set_aerial_interiors {enabled: false}`)
draws the air as before ADR-4757 everywhere, for before/after plates of a level. The fog and the
interior shade read the boxes either way. Place each box over the interior's air (its faces on the inner wall surfaces), not over open
shafts or trenches that see the sky: the air in a skylight's shaft or an open ramp is outdoor air.

Not changed: sky pixels seen from inside retain the sky table's whole-ray result, and the forward pass's analytic haze (`FogSettings`,
the default look without the physical atmosphere).

## Agent protocol

`render.aerial_interiors {direction?, distance?}` (read-only) reports what the composite read for
the last extracted frame:

```json
{ "enabled": true, "slots": 4, "volumes": 3, "dropped": 0,
  "bound": [{ "slot": 0, "index": 0, "center": [0, 5.5, 0], "half_extent": [9, 5.5, 16], "used": true }],
  "eye": [-5, 1.7, -13.5], "eye_inside": true, "eye_slots": [0],
  "ray": { "direction": [0, 0, 1], "distance_m": 100, "spans": [[0, 29.5]],
           "inside_m": 29.5, "outside_m": 70.5, "inscatter_share_near": 0.705 } }
```

- `bound` lists the bound volumes by slot; `index` is the volume's place in extraction order. With four or
  fewer volumes slot `i` holds volume `i`, and `used` is false for a box with a non-positive or non-finite half
  extent, or non-finite center (it holds its slot but takes nothing out). With more, the four usable boxes
  nearest the eye are bound, in extraction order (no eye: the first four).
- `dropped` counts volumes not bound: a bunker that still glows blue has no box, or four nearer ones.
- With a `direction` (world space, normalized here) and a `distance` (meters, default 100) the ray
  from the primary camera's eye is clipped as the shader does: `spans` (merged, sorted, meters from
  the eye), `inside_m`, `outside_m` and `inscatter_share_near`, the share of the in-scatter kept
  (`outside / distance`: exact for a surface within the table's first 2 km, where the in-scatter
  grows linearly with distance).
- `eye` is null and no ray is clipped when the frame has no view; `enabled` is the switch below
  (null without a `RuntimeRenderer`).

`render.set_aerial_interiors {enabled}` turns the rule on or off on the runtime renderer and answers
`{ before: { enabled }, after: { enabled } }`. It applies from the next frame and is not persisted. Changing it discards the held temporal frame
and upscaler history; repeating the current value keeps that history. The setting survives a renderer
warm worker and changes made on its stand-in.

Handler errors use stable lowercase codes: `invalid_aerial_ray` for an invalid direction or distance,
`invalid_aerial_enabled` for a non-boolean switch, `aerial_frame_missing` without `ExtractedFrame`, and
`aerial_renderer_missing` without `RuntimeRenderer`. Add `RenderPlugin` for the missing resources.
The shared protocol envelope, permission and parameter gates retain their existing protocol errors.

## Checks

- `regia_render` unit tests (`interior::tests`): the clip and merge (`interior_spans_follow_the_boxes_the_ray_crosses`,
  `overlapping_boxes_are_taken_out_once_and_only_four_are_read`), the walk out of a door
  (`walking_out_of_a_door_changes_the_interior_air_smoothly`) and the nearest-four pick
  (`the_eye_in_a_fifth_passage_takes_its_air_out`,
  `the_aerial_pick_is_the_list_up_to_four_and_the_nearest_past_it`); `outdoor::tests::
  interior_slots_hold_the_list_up_to_four_and_the_nearest_past_it` holds the packed slots byte-identical
  to the list's with four or fewer volumes.
- `cargo test -p regia_render --test aerial_interior` (GPU; lavapipe in CI): a box no ray crosses
  leaves every pixel bit-identical; a sealed room's air takes no in-scatter; walking out of a door
  the in-scatter taken out follows the eye's stretch under the roof to zero, with no jump, and
  outside the frame is bit-identical to the frame without the box. Duplicate boxes count once, and
  a live toggle after temporal convergence updates the frame. With five boxes and the eye in the one
  listed fifth (`the_eye_in_a_fifth_box_takes_its_air_out`), the room's air takes no in-scatter, the
  frame is bit-identical to the room alone's, and the CPU twin keeps no in-scatter either.
- `cargo test -p regia_render --lib table_entries_and_rain_respect_interiors_and_leave_outdoors_exact`
  invokes the full/lite table entries and rain veil, checks their dispatch counters, removes roofed
  in-scatter and leaves rays outside the boxes bit-identical.
- `cargo test -p regia_cli --test lightmap_gallery` declares the gallery's interior air (the hall,
  the adit, the ramp's roofed run) and holds, per region and view, the beauty's `B/G` against the
  path tracer's within 3 % and the sun-only frame's share in the sun's shadow under 0.5 %.
  `REGIA_GALLERY_NO_INTERIORS=1` renders the before plates.

For an exact parent-shader comparison, the aerial integration test and table-entry unit test accept
`REGIA_AERIAL_BASELINE_SHADER=<parent outdoor_sky.wgsl>` and
`REGIA_AERIAL_OUTDOOR_CAPTURE=<evidence directory>`. The first loads the parent through shader reload;
the second writes five linear HDR controls and the existing four sky-golden views as raw pixel bytes.
Run each variant in a fresh process. The parent intentionally fails the interior assertions after
writing the outdoor controls. Compare the controls byte for byte; these files are evidence, not goldens.
