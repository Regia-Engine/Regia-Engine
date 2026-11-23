# Forward studio axis

The forward shader carries the studio image-light extras (ADR-2000): the image light's ground
plane (its cover in the diffuse term and its reflection), the blend of local reflection captures,
and the image-light debug views. Uniforms switch them off at run time, but on an RTX 5090 their
code alone cost every frame that never used them about 0.03-0.05 ms of mesh forward (ADR-2087).
That included every Direct Order 27 frame.

Since ADR-2308 a frame composes the forward module with those extras only while one of them is
on:

| Studio feature | Turned on by |
|---|---|
| Ground plane | `render.set_environment_ground` (a plane, and an installed light that feeds it) |
| Local captures | a baked local reflection capture |
| Image-light debug view | `render.set_debug_view` `image_light` or `image_light_occlusion` |

Any other frame composes the forward module with `STUDIO_IBL_OFF`. This is the define the Adreno
workaround already used (ADR-2042). A frame drawn with that module has the same pixels as one
drawn with the full module:

- every block the define drops sits behind a uniform test that is false in such a frame;
- the one statement outside such a test, the probe adjustment, stays.

`tests/studio_axis.rs` checks four frames byte for byte under both modules: a sunlit day
exterior, the product-viewer studio, the sunless gradient studio, and the image studio under the
studio authority. `tests/studio_frame_pin.rs` keeps its values.

The axis covers the per-object forward pipeline sets and the GPU-driven colour pipelines, which
draw almost all opaque meshes. Every other forward family keeps the full module in every frame:
material graphs, mask decals, lightmap receivers, thin film, subsurface, OIT, glass, the
visibility-buffer resolve and the Mobile lite tiers.

## Cost and pipelines

- While no studio feature is on, the frame builds only studio-off pipelines. That is every game
  map and every studio frame that uses none of the three features.
- Turning a feature on builds the full twin of each set the frame draws, and nothing else: the
  pipeline count of the axis doubles at most (`tests/studio_axis.rs` pins this).
- Turning it off again builds nothing.
- The game-frame warm key `forward_studio` (in `render.graph` `warm_keys`) builds the other
  module's sets ahead of time, so a toggle mid-play compiles nothing on the frame path.
- That warm runs only when the renderer has a studio environment installed or a studio feature on.
  A game map without a studio warms nothing.
- On Adreno every module is studio-off already, so the axis has a single key there.

## Methods

- `render.forward_studio` reads the axis:
  - `mode`: `auto` or `full`.
  - `env_full`: `REGIA_FORWARD_STUDIO=full` is set for the process.
  - `adreno_workaround`: the Adreno workaround fixes every module to studio-off.
  - `features { ground_plane, captures, image_light_view, any }`: the studio features on now.
  - `studio_off`: the next frame composes the studio-off module.
  - `pipelines { sets_studio_off, sets_full, gpu_color_studio_off, gpu_color_full }`: the pipelines
    built so far for each module.

```json
{"method": "render.forward_studio", "params": {}}
```

A game map after a few frames answers:

```json
{
  "mode": "auto",
  "env_full": false,
  "adreno_workaround": false,
  "features": { "ground_plane": false, "captures": 0, "image_light_view": 0, "any": false },
  "studio_off": true,
  "pipelines": { "sets_studio_off": 1, "sets_full": 0, "gpu_color_studio_off": 2, "gpu_color_full": 0 }
}
```

- `render.set_forward_studio {"mode": "full"}` keeps the full module on every frame, as before
  ADR-2308. Use it as an A/B baseline. `auto` (the default) goes back. The answer is the new
  `render.forward_studio` state. Switching builds the other module's pipelines on the next frame.

```json
{"method": "render.set_forward_studio", "params": {"mode": "full"}}
```

To A/B one binary from outside, start the process with `REGIA_FORWARD_STUDIO=full`. This forces
the full module on every renderer in the process, the way `xtask gpu-bench` runs are compared
(see `docs/perf-log.md`).

## Errors

| Code | When |
|---|---|
| `forward_studio_bad_mode` | `mode` is missing or not `auto` / `full` |
| `F0003` | no `RuntimeRenderer` in the app (a host that drives `FrameRenderer` itself calls `forward_studio_report` and `set_forward_studio_mode`) |

## From Rust

```rust,wrap
use regia_render::{ForwardStudioMode, FrameRenderer};

let mut renderer = FrameRenderer::new();
renderer.set_forward_studio_mode(ForwardStudioMode::Full); // A/B baseline
let report = renderer.forward_studio_report();
assert!(!report.studio_off);
```
