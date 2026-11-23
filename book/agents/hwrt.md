# Hardware ray tracing

Regia Engine can trace the sun's shadow per pixel (ADR-515, ADR-1149, ADR-2035). It is off in every
preset. A game or tool turns it on per profile with `RenderProfile::sun_shadow_trace`, and an
agent with `render.set_sun_shadow_trace`.

| `sun_shadow_trace` | What the frame does |
|---|---|
| `off` | The cascaded shadow map and contact shadows. Every preset's default. |
| `on` | Hardware ray queries where the device has them. Otherwise the screen-space trace. |
| `compute` | The compute BVH on any adapter. This is the reference path. It rebuilds on the CPU when a caster moves, so it is not for play. |
| `screen` | The screen-space trace on any adapter. |

What exists:

- **A probe.** At device creation the renderer checks what the adapter offers. In wgpu 30, ray
  queries are the experimental `EXPERIMENTAL_RAY_QUERY` feature, on Vulkan only, and only where
  the driver has `VK_KHR_ray_query`. The probe also records the acceleration-structure limits.
- **An opt-in.** The device asks for ray queries only when the profile says `on` before the first
  window's device is created (`regia_render::hwrt::set_device_request`; the render plugin does it
  from `RenderSettings`). Any other profile's device request is the same as before this work, and
  mesh buffers carry `BLAS_INPUT` only on a device with ray queries.
- **Acceleration-structure builds.** The frame builds one bottom-level structure (BLAS) per mesh
  from the mesh arena and a top-level structure (TLAS) from the frame's casters. It rebuilds only
  meshes that are new or changed, and only when a caster moved, appeared or left. A steady frame
  builds nothing. Input over the device's limits is refused.
- **A visibility pass** (`sun_shadow_trace` in `render.graph`). It reconstructs each pixel's world
  position from the single-sample depth and sends one ray toward the sun. It writes the texture
  contact shadows write (a pooled transient), and it runs in their place.

Each tracer writes 1 (lit) or 0 (shadowed). The frame needs a single-sample depth prepass, a sun
that casts shadows, the full forward shader and the main view. Otherwise it keeps the shadow map and
the reason says why.

## Query

`render.hwrt` takes one optional parameter, `adapters` (boolean, default false). It works without
a renderer resource, and it fails only when `adapters` is not a boolean (`invalid_params`). It
returns:

```json
{
  "probed": true,
  "requested": "on",
  "adapter": {
    "backend": "vulkan",
    "ray_query": true,
    "vertex_return": false,
    "limits": {
      "max_blas_primitive_count": 536870912,
      "max_blas_geometry_count": 16777216,
      "max_tlas_instance_count": 16777216,
      "max_acceleration_structures_per_shader_stage": 16
    }
  },
  "device_ray_query": true,
  "shadow_path": "ray_query",
  "reason": "hwrt_enabled",
  "message": "hardware ray queries are enabled on this device",
  "sun_shadows": {
    "requested": "on",
    "preset": "DesktopUltra",
    "variant": "ray_query",
    "reason": "rt_shadows_ray_query",
    "message": "sun shadows trace with hardware ray queries",
    "forward_defs": ["SUN_SHADOW_RT"],
    "trace_shader": "sun_trace_ray_query.wgsl",
    "pipeline_key_bits": 1,
    "applied": true,
    "frame": {
      "traces": 240,
      "scene_builds": 1,
      "tlas_refits": 0,
      "casters": 412,
      "triangles": 88120,
      "shared_casters": 256,
      "set_casters": 0,
      "masked_casters": 120,
      "skipped_deforming": 6,
      "skipped_masked": 0,
      "blas_built": 38,
      "blas_resident": 294,
      "instances": 412,
      "refit": false,
      "instances_moved": 0,
      "sun_maps_skipped": 0,
      "failure": null
    }
  }
}
```

(The values above are examples. Each adapter reports its own limits.)

The report describes the last render device the process created. `probed` is false before the
first device. In that case `adapter` and `reason` are null. `shadow_path` is `ray_query` only when
`reason` is `hwrt_enabled`. Otherwise it is `compute_fallback`.

| `reason` | Meaning |
|---|---|
| `hwrt_enabled` | Requested, offered, and the device has it. |
| `hwrt_not_requested` | The profile did not ask for ray queries when the device was created. This is the default. |
| `hwrt_minspec_profile` | The device is clamped to a minimum-spec profile, which never has ray queries. |
| `hwrt_no_adapter_ray_query` | The adapter has no ray queries. This covers DX12, Metal, GL, and Vulkan drivers without `VK_KHR_ray_query`. |
| `hwrt_zero_accel_limits` | The adapter offers ray queries but reports an acceleration-structure limit of zero. |
| `hwrt_device_refused` | Ray queries were requested, but the created device does not have them. |

## Sun-shadow variants

`sun_shadows` describes the variant of the active profile. Once a frame has drawn the request, the
values are what that frame chose, `applied` says whether it wrote the visibility texture, and
`frame` says what it traced. Before that, `frame` is null and the values are what the profile and the
device would give.

| `variant` | Forward shader defs | Tracer | `pipeline_key_bits` |
|---|---|---|---|
| `shadow_map` | none | none | 0 |
| `ray_query` | `SUN_SHADOW_RT` | `sun_trace_ray_query.wgsl` (needs ray queries) | 1 |
| `compute_fallback` | `SUN_SHADOW_RT` | `sun_trace_bvh.wgsl` | 2 |
| `screen_trace` | none | `sun_trace_screen.wgsl` | 3 |

Under `SUN_SHADOW_RT`, opaque and masked surfaces take their sun visibility from the visibility
texture instead of the shadow map. Blended and transmissive surfaces and ghost overrides keep the
shadow map, because they are not in the depth the trace starts from. The shadow maps are drawn
while something reads them (see [Shadow maps under the trace](#shadow-maps-under-the-trace)).

`screen_trace` keeps the shadow-map forward shader, so the sun is the shadow map times the trace. A
ray that leaves the screen counts as lit. What it would have hit is the shadow map's, so nothing
goes missing, and the march adds fine contact the cascades lose, as contact shadows do but longer.

Pipelines key on the forward module, so `ray_query` and `compute_fallback` share one set of forward
pipelines, and `screen_trace` shares the shadow map's. The visibility-buffer resolve has both modules
too.

The checks run in this order. The first one that applies gives the `reason`:

| `reason` | Variant | Meaning |
|---|---|---|
| `rt_shadows_not_requested` | `shadow_map` | `sun_shadow_trace` is `off`. This is the default. |
| `rt_shadows_secondary_view` | `shadow_map` | A texture view or an eye. Only the main view traces. |
| `rt_shadows_no_sun` | `shadow_map` | The sun casts no shadow this frame. |
| `rt_shadows_no_depth` | `shadow_map` | No single-sample depth prepass: MSAA, or a profile without one. |
| `rt_shadows_forward_lite` | `shadow_map` | The frame draws the lite forward shader, which has no visibility texture. |
| `rt_shadows_screen_requested` | `screen_trace` | `sun_shadow_trace` is `screen`. |
| `rt_shadows_screen_fallback` | `screen_trace` | `on`, and the device has no ray queries. The top-level `reason` says why. |
| `rt_shadows_screen_shared_casters` | `screen_trace` | `on`, and the frame has a shared draw that gives the trace no casters (`SharedPassDraw::trace_casters` is `None`). Its casters would not be in the traced scene, so the frame keeps the shadow map under the screen trace rather than lose their shadows. The terrain gives a proxy, so it never takes this. |
| `rt_shadows_accel_failed` | `shadow_map` | The traced scene was refused. `frame.failure` holds its code. It stays until the request changes. |
| `rt_shadows_compute_requested` | `compute_fallback` | `sun_shadow_trace` is `compute`. |
| `rt_shadows_ray_query` | `ray_query` | `on`, and the device has ray queries. |

Before any frame, `render.hwrt` can also answer `rt_shadows_no_device`: `on` with no device yet.

## What the trace holds

The traced scene holds the frame's rigid casters with shadows on, and, since ADR-2310, three more
kinds. `frame` counts each:

- **Shared draws with a proxy** (`shared_casters`). A shared draw hands the trace coarse geometry
  through `SharedPassDraw::trace_casters`. The terrain builds a tiled grid over its whole map, one
  structure per tile, at most 2 million triangles at about 2 m spacing by default. Each vertex takes
  the lowest height within half a grid step, so the proxy never rises above the drawn ground and
  the ground does not shadow itself through it. `terrain.trace_proxy` reports it (below).
- **Persistent visibility-buffer sets** (`set_casters`), such as a static scatter. Each set keeps a
  copy of its mesh's positions for this.
- **Alpha-masked casters** (`masked_casters`, `ray_query` only). Foliage cards enter as non-opaque
  geometry. A hit is accepted for a fixed share of rays (`MASKED_COVERAGE`, one half), picked by a
  hash of the ray and the triangle. A canopy then casts a dappled shadow of about the right density,
  but no leaf shapes. The texture is not read. The compute reference still leaves them out
  (`skipped_masked`).

These still cast no traced shadow:

- skinned, morphed and vertex-animated meshes (`skipped_deforming`);
- holes cut in the terrain, which still cast as ground; relief displacement; grass; and the far
  scatter impostors.

**Moving casters refit.** When the same casters move and nothing else changes, the top level keeps
its slots and its storage. Only the moved slots are rewritten (`refit` true, `instances_moved`), no
bottom-level structure is touched, and `tlas_refits` counts the frame instead of `scene_builds`.
wgpu 30 runs every acceleration-structure update as a rebuild, so the GPU still rebuilds the top
level, but that is over instances only. A caster that appears, leaves, or changes mesh rebuilds the
scene.

The traced scene rebuilds when a caster's mesh is uploaded again. A mesh submitted with key 0 is
uploaded afresh every frame, so it rebuilds its structure every frame (`frame.scene_builds` climbs
on a still scene). Give casters stable mesh keys.

## Shadow maps under the trace

Under `ray_query` and `compute_fallback`, opaque surfaces no longer read the sun's cascades. A
frame skips drawing them, and counts it in `frame.sun_maps_skipped`, when nothing else reads them:

- no blended, transmissive, x-ray, ghost or hair surface is drawn;
- no caster is left out of the trace (skinned, morphed, vertex-animated; masked on the compute
  reference), since those still cast through the maps;
- any shared draw can take its sun from the trace alone (`SharedPassDraw::takes_traced_sun`). The
  terrain can when it draws no grass and no far impostors, which read only the cascades.

When fog, clouds or the atmosphere may run, the cascade their light shafts sample is still drawn.
The first frame after a skip redraws every cascade. A frame that skips none draws what it drew
before.

## The terrain's proxy

`terrain.trace_proxy` answers `{ settings: { spacing_m, max_triangles, tile_quads }, proxy }`.
`proxy` is null while no frame traces. Otherwise it is `{ stride, spacing_m, tiles, triangles,
tile_builds }`: samples between vertices, metres between vertices, tiles, triangles, and tile
builds so far. A height edit rebuilds only the tiles it touches.
`terrain.set_trace_proxy { spacing_m?, max_triangles?, tile_quads? }` changes the settings and
answers `{ before, after, note }`. A built proxy is rebuilt the next frame that traces. Both
methods answer `F0003` when the host inserted no `TerrainTraceProxyControl`. The host inserts
`TerrainTraceProxyControl(draw.trace_proxy_handle())` with its terrain draw. Values out of range
answer `F0002` and change nothing.

```json
{"jsonrpc": "2.0", "id": 1, "method": "terrain.set_trace_proxy", "params": {"spacing_m": 4.0}}
{"jsonrpc": "2.0", "id": 2, "method": "render.set_sun_shadow_trace", "params": {"mode": "on"}}
{"jsonrpc": "2.0", "id": 3, "method": "render.hwrt"}
```

On a card with ray queries, `sun_shadows.reason` is `rt_shadows_ray_query` and
`sun_shadows.frame.shared_casters` equals `terrain.trace_proxy`'s `proxy.tiles`. Without ray
queries it is `rt_shadows_screen_fallback`.

## Switching it

`render.set_sun_shadow_trace {"mode": "off" | "on" | "compute" | "screen"}` sets the active
profile's knob and answers `{ before, after, note }`. It applies from the next frame. An unknown
mode is `invalid_params`, and an app without `RenderSettings` is `not_found`. A preset change
replaces the profile, which turns the knob off. `on` needs a device created with the knob already
on. Without one, the frame takes the screen-space trace and says `rt_shadows_screen_fallback`.

## Every adapter in the machine

`render.hwrt {"adapters": true}` adds an `adapters` list. Each entry has `index` (the number
`REGIA_GPU_INDEX` takes), `name`, `device_type` (`discrete`, `integrated`, `virtual`, `cpu` or
`other`), `backend`, `driver`, `driver_info`, `ray_query`, `vertex_return`, `limits`, and
`if_requested`. `if_requested` is the `hwrt_*` reason that a device on that adapter would get
from a profile that asks for ray queries. The probe creates a wgpu instance but no device, and it is not cheap, so
ask for it once and not every frame.

From a shell, `regia doctor --platform gpu` prints the same probe. A plain `regia doctor`
also prints it on Linux and Windows. Ray tracing is optional, so this section never fails the
doctor.

The builders have error codes of their own: `accel_ray_query_disabled`, `accel_missing_mesh`,
`accel_duplicate_mesh`, `accel_not_triangles`, `accel_too_many_primitives`,
`accel_too_many_instances`, `accel_custom_data_too_wide`, `accel_non_finite_transform`,
`accel_mesh_buffers` and `accel_refit_shape` (a refit asked for a scene that is not the last
build's; the frame builds instead, so it never reaches a report). Rust hosts see them as `AccelError::code()`.

## Checking a machine

1. Run `regia doctor --platform gpu`, or call `render.hwrt {"adapters": true}`. Look for an
   adapter whose `if_requested` is `hwrt_enabled`.
2. Start the game or a sample with a profile whose `sun_shadow_trace` is `on`. Call `render.hwrt`
   and read `reason` and `sun_shadows`.
3. If the reason is `hwrt_no_adapter_ray_query` on Vulkan, run `vulkaninfo | grep ray_query`. If
   that prints nothing, the driver has no ray queries.
4. To compare ray queries with the reference on that machine, run
   `cargo test -p regia_render --test hwrt_frame --test hwrt_shadow --test hwrt_shared_casters -- --nocapture`
   and `cargo test -p regia_terrain --test hwrt_terrain_casters -- --nocapture` (the terrain +
   scatter scene and the 100-frame refit). The cost measurements stay `#[ignore]`d:
   add `-- --ignored --nocapture`.

**What is a bug.** If `render.hwrt` reports `hwrt_enabled` for a process whose profile never asked,
report it, because the default device must never carry an experimental feature. The same goes for a
`sun_shadows.variant` other than `shadow_map` when `sun_shadow_trace` is `off`, or for a frame that
differs from a build without this feature while the knob is off. A mismatch between the ray-query
result and the reference in those tests is also a bug.

## Testing ray queries without a card

Lavapipe (Mesa 24.1 and later; 26.1.6 on the build machines) exposes `VK_KHR_ray_query`, and wgpu gives
it `EXPERIMENTAL_RAY_QUERY`, so the ray-query path runs on a CPU. Select it and check:

```sh
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json
export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json   # the loader reads either name
regia doctor --platform gpu      # expect: llvmpipe ... ray queries yes

cargo test -p regia_render --test hwrt_frame --test hwrt_shadow --test hwrt_shared_casters -- --nocapture
cargo test -p regia_terrain --test hwrt_terrain_casters -- --nocapture
cargo test -p regia_render --test hwrt_ray_query_spirv   # naga to SPIR-V + spirv-val, no device
```

Each ray-query test asks `regia_test_utils::hwrt::ray_query_device()`. The gate decides from the
returned device's own features — never from `last_device_report()`, the process-wide report of
whichever device was created most recently, which a parallel sibling test can overwrite between
this device's creation and the read. It gets a device where the adapter has ray queries, and
otherwise prints `skipped: no ray queries on <adapter>` and returns, so the same command passes on
a machine without them (read the output to see which happened). `REGIA_REQUIRE_RAY_QUERIES=1`
(the `REGIA_REQUIRE_ASSETS` precedent) turns that skip into a failure: the lavapipe gate sets it,
so a Mesa older than 24.1, a lost `VK_ICD_FILENAMES`, or a wrong adapter pick fails the gate
instead of silently passing without ray-query coverage. The tests use small scenes and fixed frame
counts; none asserts on time. The `golden_hwrt_sun_pair` bless stays `#[ignore]`d until the lead
blesses it on the lavapipe reference; the `*_cost_on_this_adapter` measurements stay
`#[ignore]`d (run them on the card being measured).

## Any-hit mask

When the sun trace is already on, an alpha-masked caster takes an any-hit alpha test. There is
no profile field for it. Opaque casters skip it: on the ray-query path they are built with the
opaque geometry flag, so a hit commits without a candidate, and on the software walk a triangle
whose coverage channel is `0` (the value an unused channel had) commits the same way. A miss,
including a candidate the any-hit rejects and nothing else confirms, stays a miss.

`frame.masked_casters` is the count of those casters in the build (`render.hwrt`, the
`sun_shadows.frame` object). The ray-query build has counted them since ADR-2310. Each one
carries `MASKED_COVERAGE` (one half) in the instance custom data, packed by
`coverage_custom_data` into 8 bits. `any_hit` in `sun_trace_ray_query.wgsl` confirms the
candidate when a hash of the ray origin, the triangle and the instance falls under that byte.
Byte 255 is fully covered and accepts unconditionally; byte 0 rejects a non-opaque candidate.

The software walk (`bvh_walk.wgsl`, the compute reference and the sample-list fallback) runs
the same rule. The fraction lives in the first corner's `w` of the BVH triangle
(`ShadowBvh::build_with_coverage`). `hwrt::any_hit` is the CPU copy the walk is checked against.
A rejected candidate does not stop the walk: an opaque triangle further along the ray still
occludes.

Limit: no mask texture is bound on either tracer, so the any-hit does not fetch one. The mask
is the per-draw coverage the engine already stores. A cutout that exists only as a texture is
not read, and the shadow is a dappled field of that density, not the leaf shapes. The lit
fraction of a half-covered card is within 0.08 of one half over its footprint (1024 texels in
`tests/hwrt_anyhit_mask.rs`; a fair coin's standard deviation there is about 0.016). An opaque
card's covered texel count stays 1024.

The frame's compute build still omits masked draws until the integration patch
`sprint-hwrt-anyhit-mask-frame.patch` is applied. That patch feeds them to
`build_with_coverage`, counts them in `masked_casters`, and stops treating them as casters the
shadow map must still draw. The walk itself already accepts the coverage channel. The
ray-query frame path already includes them.
