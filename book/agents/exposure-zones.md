# Exposure zones

A box or a capsule that asks the eye for an EV range. The renderer samples the
zones into the frame (ADR-322): every frame `ExposureZonesPlugin` (in
`DefaultPlugins`) weighs the zones at the active camera, and the main view's auto
exposure keeps the eye inside the combined range. The math is
`regia_render::exposure_zones` (`ExposureZone`, `zone_weight`,
`combine_zones`, `adapt_ev`, `ExposureLog`, `ZoneExposure`). See the
[DO27 host checklist](../user-guide/do27-host-checklist.md). Mesh bores stay on
[Tunnels](tunnels.md).

## Volume

`ExposureZone` is a component. Put it on an entity that already has a
`GlobalTransform`. The shape is local, in meters:

- `ExposureZoneShape::Box { half_extents }` — half size on local X, Y and Z.
- `ExposureZoneShape::Capsule { radius, half_height }` — capsule along local Y.
  `half_height` is half the cylindrical segment. Each cap adds `radius`, so the
  total length is `2 * (half_height + radius)`.

`ev_min` and `ev_max` are the EV100 range the zone wants. A zone that wants one
exposure sets both ends equal. `blend_distance` is the width of the fade, in
meters. `adapt_up_speed` and `adapt_down_speed` are EV per second (toward a
higher EV, and toward a lower one). `priority` is an `i32`: the higher value wins
when two zones both cover the eye. The component has no serde derive, the same as
`InteriorVolume`.

The default is a 1 m box at EV 14.5, a hard edge, 2.0 EV/s up, 1.5 EV/s down,
priority 0.

## Mouth blend

Signed distance is positive outside and negative inside, in meters. The box and
the Y-aligned capsule follow Inigo Quilez's public-domain distance functions.
The blend band sits outside the surface:

```text
weight = 0    at blend_distance meters outside
weight = 1    on the surface, and everywhere inside
between       Hermite smoothstep, t * t * (3 - 2 * t)
```

A tube only a few meters across can still fade across a 25 m mouth, because the
band does not have to fit inside the bore. A blend of 0 is a hard step: weight 1
for a distance of 0 or less, otherwise 0.

A rigid transform, or a per-axis scale, keeps the box distance exact. A capsule
is exact when the local X and Z scales match. A sheared matrix is orthogonalized
first.

## One target

`combine_zones(&[(weight, &zone), ...], default)` returns an `EvTarget`.
`combine_weighted_zones` is the same for zones held by value (a reused scratch
vector).

- An empty list, or a list with nothing usable, returns the sanitized default
  range and the default speeds (2.0 EV/s up, 1.5 EV/s down).
- The highest priority with a positive finite weight wins. Lower priorities drop
  out.
- Winning weights saturate to 1. When they sum past 1 they are renormalized.
  When they sum to less than 1, the remainder is `default`. That is how one
  mouth narrows the range as the camera approaches.
- Non-finite inputs are dropped. The result is always finite.

The framework passes `ZONE_OPEN_RANGE` as the default: EV100 −2 to 15, the
default auto-exposure band. At the outer edge of the band the combined range is
that open band, and on the face it is the zone's own range.

## How the frame uses it

Each frame, after transform propagation, `sync_exposure_zones`:

1. takes the active camera: the window camera with the lowest `order` (the one
   the renderer draws first). Texture cameras are never zoned;
2. collects every zone with a positive weight at the camera's eye;
3. combines them with `ZONE_OPEN_RANGE` and calls
   `RuntimeRenderer::set_exposure_target(Some(target))`, or `None` when no zone
   covers the eye;
4. hands the frame's `Time` step to the renderer as the adaptation `dt`.

The main view's auto exposure then runs through `ZoneExposure`:

- **Metering.** The eye chases
  `clamp(metered, ev_min - compensation, ev_max - compensation)`
  (`zone_goal_ev`). `metered` is the newest histogram on its own (not the
  smoothed meter), after the auto-exposure band clamp and the bright-subject
  bias. A bright window inside a tunnel still meters, but it cannot pull the eye
  out of the range. Exposure compensation shifts the result as it does outside
  a zone. A zone may ask for an EV outside the level's auto-exposure band: its
  clamp comes after the band's.
- **Adaptation.** One `adapt_ev` step per frame with the target's speeds,
  starting from the EV the frame was about to apply, so entering a band never
  jumps. The usual luminance smoothing is not applied on top.
- **Leaving.** When the target clears, the eye keeps walking toward the
  smoothed meter at the default speeds and hands back once it is within 0.05 EV
  (`ZONE_RELEASE_TOLERANCE_EV`). From then on the frame is exposed exactly as
  without zones.
- **No zones.** With no target and nothing to hand back, the exposure is bit
  for bit the plain auto exposure. A map without zones renders the same pixels
  as before, so no golden image changed.
- **Manual exposure ignores zones.** A view with auto exposure off, or a camera
  with `exposure_ev`, is exposed at its fixed EV. The eye is dropped, so turning
  auto exposure back on starts from the meter.
- **Camera cut.** A cut that snaps exposure (`render.camera_cut`, the default)
  starts the next zone frame on its goal. A fading cut keeps adapting.
- **dt.** The host's frame step (`FrameRenderer::set_exposure_dt`, which the
  framework sets from `Time::delta_secs` every frame). A frame with no step, a
  step of 0, or a non-finite step holds the eye. The ease does not read the wall
  clock: a replay that presents the same frames with the same steps adapts the
  same way.

A host that drives `FrameRenderer` itself calls `set_exposure_target` and
`set_exposure_dt` before `render_into`.

## Adaptation

`adapt_ev(current, target, dt, up_speed, down_speed)` is one step of
`dy/dt = speed * (target - y)`:

```text
next = current + (target - current) * (1 - exp(-speed * dt))
```

`dt` is clamped to the closed interval from 0 to 0.1 seconds
(`ADAPT_DT_CAP_SECONDS`). The factor stays in 0..=1, so the step lands on the
segment between `current` and `target` and can sit on the target. Moving to a
higher EV uses the up speed, to a lower EV the down speed. A non-positive speed
holds. A non-finite argument returns `current` unchanged.

`adapt_settle_steps(gap, tolerance, speed, dt)` is how many steps bring a gap
within a tolerance: `ceil(ln(gap / tolerance) / (speed * dt))`. From sun (EV
14.5) to the tube (EV 8.5) at 1.5 EV/s and 30 fps, to within 0.05 EV, that is
96 steps (3.2 s). Back out at 2.0 EV/s it is at most 74 steps.

## Agent protocol

| Call | Result |
|---|---|
| `render.get_exposure` | `{ ev, goal, meter_ev, mode, adapting, settled, zone_target, zone_count, metered_luminance, logging, log_samples, zones }` |
| `render.exposure_log {"action": "start", "cap"?: n}` | `{ logging: true, cap }`, a fresh log (default cap 4096, at most 262144) |
| `render.exposure_log {"action": "stop"}` | `{ logging: false, count }`; the samples stay readable |
| `render.exposure_log {"action": "get"}` | `{ logging, count, last, samples: [{ time, ev, target }] }` |
| wait atom `{"predicate": "exposure_settled", "tolerance"?: 0.05}` | holds when `|ev - goal| <= tolerance` |

`render.exposure_log` starts and stops a log, so it needs the `play` permission and counts as a mutating call
(ADR-3285); `render.get_exposure` stays `read`.

All EVs are EV100 with exposure compensation applied. `mode` is `meter` (plain
auto exposure), `zone`, `release` (handing back) or `manual`. `goal` is what the
eye chases: the clamped meter in a zone, the smoothed meter while handing back,
the newest histogram in `meter` mode. `adapting` is true while a zone (or its
release) is more than 0.05 EV from its goal. `zone_target` is
`{ ev_min, ev_max, target_ev, adapt_up_speed, adapt_down_speed }` or `null`.
`zone_count` counts the zones with a positive weight at the eye, every priority,
and is `null` without `ExposureZonesPlugin`. Before the first frame `ev`, `goal`,
`meter_ev`, `mode` and `settled` are `null`. `zones` is the baked section list
(see [Zones from the bake](#zones-from-the-bake)): `null` when the host has not
inserted `BakedZoneList`, otherwise one object per cooked section.

The log records one sample per main-view frame, zone or not: `time` is the sum of
the frame steps since `start` (deterministic under `play.step`), `ev` the
exposure and `target` the goal. Past the cap the oldest sample drops.
`exposure_settled` fails (and a `wait_until` keeps polling) before the first
exposed frame.

Errors carry `data.reason`:

| Reason | Code | When |
|---|---|---|
| `renderer_missing` | F0003 | the app has no `RuntimeRenderer` |
| `invalid_action` | F0002 | `action` is missing or not `start`, `stop` or `get` |
| `invalid_cap` | F0002 | `cap` is not an integer from 1 to 262144, or given without `start` |
| `invalid_tolerance` | wait error | `tolerance` is not a number above 0 |

## A tunnel

```rust
use regia_render::exposure_zones::{ExposureZone, ExposureZoneShape};

/// The DO27 tube: 400 m long, entered along +Z, EV 8.5 inside, a 25 m mouth.
fn tube_zone() -> ExposureZone {
    ExposureZone {
        shape: ExposureZoneShape::Box {
            half_extents: [8.0, 4.0, 200.0],
        },
        ev_min: 8.5,
        ev_max: 8.5,
        blend_distance: 25.0,
        priority: 1,
        ..ExposureZone::default()
    }
}
```

Spawn it with a `Transform` at the tube's center, rotated so local Z runs down
the bore: `commands.spawn((Transform::from_translation(center), tube_zone()))`.
A side chamber that should stay brighter is a second, smaller zone with a higher
`priority`.

The tunnel acceptance scene (`crates/regia_terrain/tests/tunnel_scene`, ADR-3257, ADR-8629) drives a
car in and out of an 80 m tube at 20 m/s (2 m a frame at 10 fps) with a zone over the tube from 15 m in
to the end cap: the cook's band for a tunnel section (EV 8.5 to 10.5, a 25 m blend; spec 121 § 5.6),
8 EV/s both ways. Starting the box inside the tube puts the portal in the blend band, so the range keeps
narrowing while the camera crosses the portal. The zone tests drive to 60 m in: the tube lit by its bake
meters inside the band for the first 40 m or so and under it beyond, where the zone holds the eye on the
band. Against the same drive without it the zone lowers the worst frame-to-frame step of the mean luma
entering the tube (14.9 against 20.2 levels) and leaves the drive's worst no higher (14.9 against 20.2;
`exposure_zone_eases_the_mouth`), and no step passes 25 (`exposure_zone_removes_the_entry_flash`). The
eye still chases the newest histogram, which is two frames old. With the default speeds (2.0 and 1.5
EV/s) this drive's entry eases by at most 1.3 levels: at 2 m a frame the roof darkens the picture in one
frame. A zone whose band does not bind (a tube that meters inside it) only makes the eye faster than the
plain meter, which can make a picture change inside the tube step harder than without the zone (27.5
against 20.2 levels at a turnaround 40 m in, ADR-8629). The scene hands the zone's target to
`FrameRenderer::set_exposure_target` with the path's frame step as `set_exposure_dt`, as
`regia_framework::exposure_zones` does with the frame's `Time`.

## Scripted drive

The acceptance drives are unit tests in `regia_render::exposure_zones`:

- `scripted_tunnel_drive_has_no_flash_or_blackout` (ADR-257): the zone math
  alone, sun EV 14.5 as the default range.
- `zoned_tunnel_drive_stays_on_the_segment_and_settles` (ADR-322): the
  renderer's eye. 30 fps for 40 s at 20 m/s through the box above. The meter
  reads EV 14.5 outside and EV 5 inside the bore. Every frame the EV stays
  between the previous EV and the goal, moves at most 0.5 EV, and never leaves
  [8.5, 14.5] (the EV 5 meter never reaches the eye). 96 frames after the portal
  it is within 0.05 EV of 8.5 and stays there to the far portal. After the far
  band it hands back to the meter at exactly EV 14.5.

The same checks against a running game: `render.exposure_log {"action":
"start"}`, drive the camera with `play.step`, then `render.exposure_log
{"action": "get"}` and assert on `samples`. From the frame the camera enters the
first blend band until the eye hands back, each `ev` lies between the previous
sample's `ev` and its own `target`, and no step exceeds 0.5 EV at 30 fps. Outside
the zones (`mode` `meter`) the frame follows the plain smoothed meter and
`target` is the newest histogram, which the smoothed value trails. `wait_until
{"predicate": "exposure_settled"}` waits for the eye to arrive.

## Log

`ExposureLog` records `(time, ev, target)` up to a cap (default 4096) and drops
the oldest sample. `to_json` writes one object:

```text
{"samples":[{"time":0.000000,"ev":14.500000,"target":14.500000}]}
```

```rust
use regia_render::{ExposureZone, ExposureZoneShape, EvRange, adapt_ev, combine_zones};
use regia_scene::GlobalTransform;

fn exposure_at_the_mouth() -> f32 {
    let zone = ExposureZone {
        shape: ExposureZoneShape::Box {
            half_extents: [8.0, 4.0, 200.0],
        },
        ev_min: 8.5,
        ev_max: 8.5,
        blend_distance: 25.0,
        priority: 1,
        ..ExposureZone::default()
    };
    // Halfway across the 25 m shell in front of the mouth at z = -200.
    let eye = [0.0_f32, 1.5, -212.5];
    let weight = zone.weight_at(&GlobalTransform::IDENTITY, eye);
    let target = combine_zones(&[(weight, &zone)], EvRange::point(14.5));
    adapt_ev(
        14.5,
        target.target_ev(),
        1.0 / 30.0,
        target.adapt_up_speed,
        target.adapt_down_speed,
    )
}
```

The post stack's exposure and tonemap settings themselves (auto exposure on or off, fixed EV, tonemap
operator, physical camera) are `render.get_exposure_settings` / `render.set_exposure_settings`: see
[Exposure and tonemap](exposure.md).

## Zones from the bake

A cooked section (spec 121 § 5.6) is one exposure zone, one reverb zone and one interior volume.
`regia_world_geom::zones::section_volumes` reads the cooked sections and zones and returns one
`SectionVolumes` per section, in section order. It does not stop at four. The aerial composite still
packs four interior slots (the four nearest the eye). The zone list is the full bake.

`ExposureEase::step(target, dt_s)` eases toward `target` by the frame clock's `dt`. The same `dt`
sequence gives the same exposure run to run. A step of 0, or a non-finite step, holds the previous
value. The first sample, with no previous value, takes the target. There is no snap when the eye
moves a long way in one step. `ExposureEase::cut` is the explicit snap a host asks for; a drive
through a mouth does not call it.

`EaseSettings::depth_blend` defaults to on. The under-rock weight then follows a smoothstep from 3 m
to 22 m of overburden, including under a lid, so crossing a mouth does not write the interior floor
in one sample. Off, the weight is the shell's step: 0 at or below 2 m, 1 under a lid or outside an
open cut, and 0 on an open cut.

A host that has cooked a map builds the rows with `section_volumes` and inserts
`regia_agent::BakedZoneList`. `render.get_exposure` then returns `zones`, an array of
`{ section, exposure_ev, blend_m, reverb_preset, reverb_size_m, interior_center_m, interior_half_m }`.
Without the resource, `zones` is `null`. The array is not capped at four.

The acceptance drive walks each Switchbacks mouth at 2 m and a frame step of 0.25 s. The exposure
change per sample is at most 0.5 EV. The outdoor and interior floors in that test are the caller's
(11.5 and 9.0). They are not engine defaults. The Switchbacks cook has 37 sections (the plan's name
for the test says 35; the count is the cook's, spec 121 § 5.6).
