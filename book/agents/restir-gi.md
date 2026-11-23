# ReSTIR GI (hardware ray queries)

ReSTIR GI traces one cosine-distributed indirect ray per visible pixel, shades its hit from the
existing probe field and a shadow-tested sun, reuses path reservoirs temporally and spatially,
then runs the GI denoiser. It is a Desktop Ultra feature behind `RenderProfile::restir_gi` and is
**off in every preset**. The SDF-traced probes (the DDGI / Depth tier) remain the fallback. No
preset, default frame or golden changes when the switch is off (ADR-3349).

## Enable it before creating the device

Set the profile switch before the first window/headless device is created. Device creation then
requests `EXPERIMENTAL_RAY_QUERY` and, where offered,
`EXPERIMENTAL_RAY_HIT_VERTEX_RETURN`. Enabling after device creation cannot add either feature;
the frame keeps the probe tier and reports the reason.

```json
{"jsonrpc":"2.0","id":1,"method":"render.set_restir_gi","params":{"enabled":true}}
{"jsonrpc":"2.0","id":2,"method":"render.restir_gi","params":{}}
```

A preset change replaces the profile and turns the switch off. The runtime's preset recognition
ignores the opt-in, so enabling it does not move unrelated terrain, foliage or water tiers.

The pass needs the main view, single-sample depth, a linear HDR target, an active probe field and
a built TLAS. The probes still update while ReSTIR GI is active: they supply hit radiance and the
fallback. Only the final DDGI screen lookup is replaced before the forward pass.

## Read the result

`render.restir_gi` returns:

- `enabled`: the active profile's switch.
- `preset`: the recognized preset name; runtime opt-in does not make it custom.
- `device`: `ray_query`, `vertex_return`, the HWRT reason and message, or null before the first
  device probe.
- `settings`: one initial ray per pixel, `temporal`, `history_cap`, `spatial_taps`,
  `spatial_radius_px`, `spatial_cap`, surface tolerances, `validation_interval`, and denoise
  settings.
- `frame`: null without a renderer. Otherwise:
  - `active` and `path` (`restir_gi` or `sdf_probes`);
  - `fallback`: null while active, else `{path:"sdf_probes", reason, message}`;
  - `frames`, `resets`, `resource_builds`, `bind_group_builds`, lookup `size`, and `last_error`;
  - `history.valid`: the next active frame can reproject reservoir and denoiser history;
  - `history.resets`: cumulative reset count;
  - `work`: the newest completed GPU counters, or **null** before asynchronous readback;
  - `total_work`: the sum of all completed readbacks.

`work` and `total_work` contain `rays`, `initial_rays`, `shadow_rays`, `visibility_rays`,
`temporal_reused`, `spatial_reused`, `history_rejections`, `validation_rejections`, `initial_hits`,
`sky_samples`, `sun_visible`, `probe_lum_sum` (whole-unit Rec. 709 sum at initial hits), and
`motion_rejections`.
Null does not mean zero work. Counters can be a few frames late and never block a frame.

### Fallback reasons

| Reason | Why the probe tier was kept |
|---|---|
| `off` | The profile switch is off. |
| `no_ray_query` | The device has no hardware ray queries. |
| `no_vertex_return` | The device cannot return hit triangle vertices. |
| `secondary_view` | `restir_gi_fallback` returns this when asked with `main_view: false`. The frame recorder returns before that on a texture view and does not write it: the report stays the last main view (or the default `off` when no main view has run). |
| `multisampled` | The depth buffer has more than one sample, or the depth prepass is off. A one-sample frame with the prepass off is reported as `multisampled`: the frame sets `single_sample` only when `msaa_samples == 1` and the prepass ran. |
| `not_linear_hdr` | The pass needs the linear HDR target. |
| `no_probes` | The probe field is not active; the existing indirect path is unchanged. |
| `no_tlas` | No usable top-level acceleration structure was built. |
| `invalid_input` | A frame input or preparation step failed; `last_error` has details. |

Fallback is a valid result, not an RPC error. In particular, a device without ray queries draws
the ordinary probe frame; it does not silently run the compute BVH as ReSTIR GI.

## Change reuse or reset history

All setter fields are optional, but a call must name at least one:

| Field | Values | Effect |
|---|---|---|
| `enabled` | boolean | Opt in or return to the probe tier. |
| `temporal` | boolean | Enable or disable temporal reservoir reuse. |
| `spatial_taps` | integer 0–8 | Neighbor reservoirs per pixel (default 4; 0 disables spatial reuse). |
| `history_cap` | number 1–500 | Temporal reservoir sample cap (default 30). |
| `reset_history` | boolean | `true` resets reservoirs and denoiser history on the next active frame. |

```json
{"jsonrpc":"2.0","id":3,"method":"render.set_restir_gi","params":{"temporal":true,"spatial_taps":2,"history_cap":12}}
{"jsonrpc":"2.0","id":4,"method":"render.set_restir_gi","params":{"reset_history":true}}
```

The answer is `{before, after, note}`, with the same state shape as `render.restir_gi`.
An unchanged setting does not reset history. A changed reuse setting resets it once on the next
active frame. Camera cuts and resizes also reset it. Settings and explicit resets require a
`RuntimeRenderer`; changing only `enabled` requires just `RenderSettings`.

## Errors

| Code | Meaning |
|---|---|
| `F0002` | Bad parameters: a wrong type, unknown field, empty setter, `spatial_taps` outside 0–8, or `history_cap` outside 1–500. Nothing changes. |
| `F0003` | `RenderSettings` is missing, or a reuse-setting/history call has no `RuntimeRenderer`. Nothing changes. |

There is no `restir_gi_unavailable` error: unsupported hardware reports `frame.fallback`.

## Verify a frame

1. Enable before device creation and call `render.restir_gi`.
2. Confirm `device.ray_query` and `device.vertex_return` are true and `frame.active` is true.
3. Wait for several active frames; do not use wall-clock sleeps. A host test can render a fixed
   number of frames and then read the method again.
4. Check `history.valid`, counters and `fallback == null`. Wait for `work != null` when measured
   counters are required.
5. For a steady-state check, compare `resource_builds` and `bind_group_builds` across further
   fixed frames after warm-up. They must not increase with unchanged size and input objects.

## Limits

The estimate is intentionally biased: spatial normalization follows the papers' practical form,
and the resolve pass clamps the estimate's luminance to twice the primary surface's own probe
lookup. The cache is the pass's only radiance source, so the guard bounds a single diffuse
bounce by the local cache estimate; it exists because probes embedded in thin walls leak direct
light that a path-vertex lookup can read while a surface lookup reads dark (ADR-3349). In
sunlit thin-walled scenes the clamp can dim legitimately bright one-bounce paths.

The lavapipe validation harness uses small fixed scenes, fixed seeds and fixed frame counts. Its
command is in the lane report and in `crates/regia_pt/tests/restir_gi_validation.rs`.

## Object motion

A temporal reservoir stores its hit in world space. Camera motion is still the receiver
reprojection (previous UV minus current UV, then the depth and normal test). That test does not
see an object that moves while the receiver stays still: the connection to the old hit is empty
and stays unoccluded, so the previous frame's hit would be kept.

An opaque hit (`hit` instance id at least 1) is dropped when the surface at that world point has
moved farther than `HIT_MOTION_METRES` (0.05 m, ADR-4001). The confirm ray starts that far outside
the stored point along its outward normal and returns along the normal. A miss, a hit past the
threshold, or a different instance rejects the reservoir and increments `motion_rejections`. Those
rejects are also counted in `history_rejections`. A sky sample is left to the camera test. A still
surface keeps the reservoir when only the camera moves. The energy guard, the constant-per-draw
hit material and the probe radiance source are unchanged.


The reservoir also stores the selected hit in object space. If the same instance still covers the
old hit, its current transform moves that original material point back into world space. A
Euclidean displacement above 0.05 m rejects reuse, including motion tangential to an overlapping
surface. Camera motion does not enter this world-space displacement. Vertex deformation with
an unchanged instance transform is not tracked. This adds 16 bytes per transient reservoir
(96 bytes total); there is no persisted format change.

The object-motion acceptance fixture measures reservoir behavior with denoise `alpha_color = 1`
and no à-trous iterations. The production denoiser settings are unchanged; its separate temporal
filter can retain earlier lighting even after a reservoir has been rejected.

Work readback samples completed frames; `total_work` sums those snapshots. A rejection confined
to one frame can fall between snapshots. To verify `motion_rejections`, use a fixed sequence
of sustained object motion and read the completed work after several frames. The single-move
image acceptance remains a separate frame-0/frame-1 comparison.
