# Blue noise

Regia Engine can drive a frame's stochastic passes from a precomputed spatiotemporal blue-noise
table. It is off in every preset. A game or tool turns it on per profile with
`RenderProfile::blue_noise` (`FrameQuality::blue_noise`), and an agent with `render.set_blue_noise`.

## The table

`regia_render`'s build script generates the table: 16 slices of 64 × 64 bytes, 64 KiB in all. The
generator is integer arithmetic plus IEEE additions and multiplications, so every host makes the same
bytes, and a test compares the embedded table with a fresh run and pins its hash.

- Every slice holds every byte value equally often, with a blue spectrum: a pixel's neighbors hold
  far-away values.
- Every pixel's sequence along time is blue too, so a 16-frame mean converges faster than independent
  numbers do.

## Where it goes

| Pass | With the knob on |
|---|---|
| Stochastic SSR (`sssr_intersect.wgsl`) | The two numbers of each ray's lobe sample. |
| Screen-space GI (`ssgi.wgsl`) | The gather spiral's rotation. |
| SSAO (`ssao.wgsl`) | A jitter of the kernel's rotation within a sixteenth of a turn; the rotation itself stays the interleaved gradient noise (below). |
| Forward pass (`lab_forward.wgsl`, ADR-2652) | The sun cascade and local shadow kernel rotation, the hashed alpha threshold, the pane SSR start offset. |

The slice is the pass's noise index modulo 16: the frame counter for SSR, and the temporal noise index
for SSGI and SSAO, which stays 0 without temporal AA. So a frame is a deterministic function of its
index.

SSAO blurs its kernel over 4 × 4 texels, and the interleaved gradient noise it draws with the knob
off is a lattice made for such a window. So with the knob on the kernel keeps that lattice and takes
the table's number as a jitter within a sixteenth of a turn (ADR-5457). With the table's number as
the whole rotation the texels under the blur were independent draws, and a frame lit by the probes
(where the occlusion scales most of the indirect light) flickered 16 % more with the knob on than
off; now it flickers 15 % less and has a tenth less grain (`tests/blue_noise_frames.rs`, RTX 5090).

The temporal resolve draws no random numbers of its own. Its history is 16-bit float and its jitter is
one camera offset per frame. What it gains is that SSGI, SSAO and SSR hand it noise that averages out
over time.

The GI denoiser (`regia_render::gi_denoise`, which the frame does not run) filters whatever noise
it is given and draws no numbers of its own. `seeing.gi_denoise_check { blue_noise: true }` draws a
scenario's input from the table instead of a hash, which is what a blue-noise gather would hand the
denoiser.

## What it costs

Off, nothing: the texture is not created, and every pass binds and runs what it always did. On, one
64 KiB texture and one more pipeline per pass, bound through a second bind group, with the same
per-pixel work plus a texture read. The frame warm builds them (the `blue_noise` warm key), so a
profile that turns it on creates nothing during play.

The forward pass reads the table from its view group (binding 38) on desktop devices: every forward
module of such a device declares it, so the depth prepass and the colour pass draw the same hashed
alpha threshold. With the knob off the binding holds a 1×1 stand-in and the forward pass computes the
interleaved gradient noise it always did (the default frame is bit-identical). The hashed alpha
threshold takes the table value directly (the slices step through time), where interleaved gradient
noise is advanced by the golden ratio each frame. Android devices keep the old layout and the old noise
(`forward_binding` is `false`).

## What it does not do

The pane's rough transmission keeps interleaved gradient noise, and so does the LOD cross-fade dither.

## Agent methods

`render.blue_noise` returns:

```json
{
  "requested": false,
  "frame": false,
  "sites": ["sssr_rays", "ssgi_rotation", "ssao_rotation", "forward_shadow_rotation", "forward_hashed_alpha", "pane_ssr_offset"],
  "forward_binding": true,
  "table": { "size": 64, "slices": 16, "bytes": 65536, "revision": 1, "hash": "…" }
}
```

`requested` is the active profile's knob. `frame` is whether the last frame used the table, `null`
without a renderer. `forward_binding` is whether this device's forward pass can read it (`null` before
the first frame).

`render.set_blue_noise { enabled }` switches the knob from the next frame and returns
`{ before, after, note }`. A preset change replaces the profile and turns it off.

Errors: `F0002` when `enabled` is not a boolean; `F0003` without `RenderSettings`.

Captured on lavapipe: calling `render.set_blue_noise` with `{"enabled":"yes"}` fails with
`F0002`. The CLI's JSON `data` value is:

```json
{"code":"F0002","expected":"a boolean","hint":"check the method's parameters in the agent guide","param":"enabled"}
```

## Workflow

In a running render app, record the current reading, enable the profile knob, then read it again
after the next frame. Confirm `requested` is `true`; `frame` reports whether that frame used the
table, and `forward_binding` reports whether this device has the forward-pass binding. Turn the
knob back off when the comparison is done:

```sh
regia agent call render.blue_noise '{}' --project <project>
regia agent call render.set_blue_noise '{"enabled": true}' --project <project>
regia agent call render.blue_noise '{}' --project <project>
regia agent call render.set_blue_noise '{"enabled": false}' --project <project>
```
