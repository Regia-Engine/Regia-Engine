# Degenerate cameras and NaN guards

What the renderer does with inputs that would otherwise turn a frame into NaN, and the log lines that
tell you it happened (ADR-869). None of this has an agent method of its own: you see it in
`seeing.screenshot` output and in the log (`logs.read`).

## Cameras the frame repairs

Before any matrix is built, `FrameRenderer::render_into` checks the frame's camera and renders a
repaired copy when one of these is true. The first repair in a process logs one warning
(`unusable camera (eye … target … fov_y … near … far …); rendering with …`).

| Input | Rendered as |
|---|---|
| `far = f32::INFINITY` | far 100 km (`view_math::INFINITE_FAR_STANDIN`); depth is standard Z, not reverse-Z |
| `far` NaN, `−∞` or not beyond `near` | `near × 10⁴` |
| `far` beyond 1e9 m | 1e9 m (`view_math::MAX_FAR`) |
| `near` NaN, infinite, zero or negative | 0.1 m |
| `near` below 1 µm or above 1000 km | clamped to that range |
| `fov_y` NaN | π/4 |
| `fov_y` outside (1e-4, π − 1e-4) | clamped to that range |
| eye NaN or infinite | the origin |
| target NaN or infinite, or on the eye | one meter ahead along −Z |

A camera looking exactly along the up axis (straight down at a map, straight up at the sky) is not a
repair: the view keeps its direction and takes −Z as screen up looking down (+Z looking up), the
picture a camera pitched to the pole from −Z would show. The same holds for a sun at the zenith in
the sun cascades. Before this, all of these drew a black or garbage frame.

## Camera fields the 3D frame does not draw yet

They log one warning the first time an extracted camera uses them, and the view still renders:

- `Camera::orthographic` / `ortho_size`: the frame draws the camera in perspective with `fov_y`.
  Picking (`screen_ray`) already uses parallel rays for it, so picks and pixels disagree.
- `ClearMode::Color(c)`: the frame clears as for `ClearMode::Skybox`.
- `Camera::hdr` and `Camera::msaa_override`: not read; the render profile decides.

## History that cannot be poisoned

Four passes carry results from frame to frame. Each replaces a NaN or infinite value before storing
it, so one bad pixel or probe recovers on the next frame instead of staying for good:

- the GPU probe volume (DDGI) irradiance and distance moments (a bad probe restarts from black);
- the cloud reconstruction (a bad texel restarts as clear sky);
- the froxel fog integration (a bad froxel is clear air);
- the auto-exposure average (a non-finite meter reading is skipped).

TAA, TSR, FSR 3 and stochastic SSR already guarded their histories.

## Checking a scene

- A black frame with a camera straight up or down, or with an infinite far plane, is no longer
  expected. If you see one, capture it and read the log for the warning above.
- A render graph built with `RenderGraph` reports a dependency cycle or a resource written by two
  nodes through `RenderGraph::try_compile`; `compile` logs the same error and runs what it can.
