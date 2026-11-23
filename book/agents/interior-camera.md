# Interior camera

The interior chase camera is presentation. It shortens a boom under a lid, holds a mouth zone
across a junction, and keeps the eye out of rock. It is not a lockstep tick. Stage A still has its
own floor and clip copies; this profile only moves the presented eye (ADR-8336).

| Method | Permission | What it does |
|---|---|---|
| `camera.interior_state` | read | Zone, blend, boom length, and the round-trip `state`. With `dt_s`, steps that frame. With `pivot` and `desired` together, also returns the blend-shortened eye. It does not move a scene camera and it does not sweep geometry. |

`{}` reports the defaults and does not consume the spawn cut: zone `exterior`, blend 0, boom 4.5 m,
`seed` true. A later call passes the previous `state` object back.

## Parameters

| Name | Meaning |
|---|---|
| `dt_s` | Frame duration in seconds. Absent: report `state` and do not step. Zero keeps the blend. Negative or non-finite is `interior_bad_dt`. |
| `under_lid` | The body is under a lid this frame. Default false. Used only when `dt_s` is present. |
| `near_mouth` | The body is in a mouth band this frame. Default false. Used only when `dt_s` is present. |
| `exterior_boom_m` | Open-ground boom the blend shortens, metres. Default 4.5. Negative or non-finite is `interior_bad_boom`. |
| `pivot` | Look pivot, three metres. Pass with `desired`, or pass neither. One of the pair is `interior_bad_state`. |
| `desired` | Rig eye before the shorten, three metres. Pass with `pivot`, or pass neither. |
| `state` | Previous result's `state` object. Omitted fields start from a fresh exterior profile (`seed` still true). |

The interior boom default is 2.5 m. The blend moves by at most `dt_s / 0.5` after the spawn cut, so a
mouth takes half a second of frame time. An interior zone that loses its lid becomes a mouth and
returns to exterior only after 30 open frames. The first outdoor frame consumes the spawn cut.
`under_lid` or `near_mouth` on that first frame snaps the blend; later frames do not.

`pivot` and `desired` together also return `eye` and `look`. `look` stays at `pivot`. `eye` is the
blend shorten, with no lid cap and no wall sweep. A host that has a cooked world places the eye
with `InteriorProfile::place` (`regia_world_geom`, feature `stage_b`): a sphere march, `segment`
against walls and roofs, then `push_disc` while the sphere still touches. A pull-in follows a 0.06 s
time constant and stops at the last clear point on that span. It does not jump the eye through rock
to the clip in one frame.

## Result

`zone` is `exterior`, `mouth`, or `interior`. `blend` runs from 0 to 1. `boom_m` is the boom length
in metres. The same fields are repeated under `state`, with `open_ticks`, `covered`, `seen_inside`,
`exiting`, `gap`, `seed`, `cover_timer_s`, and `passthrough`. `eye` and `look` are present only when
`pivot` and `desired` were both passed.

## Error reasons

`data.reason` on `camera.interior_state`:

| Reason | Meaning |
|---|---|
| `interior_bad_dt` | `dt_s` is negative or not finite. |
| `interior_bad_zone` | `state.zone` is not `exterior`, `mouth`, or `interior`. |
| `interior_bad_boom` | `exterior_boom_m` or `state.boom_m` is negative or not finite, or the shaped boom is not finite. |
| `interior_bad_state` | `pivot` and `desired` were not passed together, a `state` field is out of range, or the shaped eye is not finite. |

A wrong JSON type names `param`. A field inside `state` is named `state.zone` (and the same for the
other fields), not only the inner name.

## Limits

The agent method does not query the world. Ceiling clamp, sphere sweep, and junction easing run in
the presentation profile, on the frame clock. Defaults are presentation tuning (`CameraInteriorParams`):
margin 0.35 m, cover 3 m, exterior boom 4.5 m, interior boom 2.5 m, mouth blend 0.5 s, sphere 0.5 m,
floor reach 12 m. A floor farther below the caller than that reach is ignored, so a chase on the
plain does not pick a tube 30 m down. The profile is not a simulation input.
