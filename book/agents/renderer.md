# Renderer: scale, culling and output

The frame path is `FrameRenderer`. Quality comes from `RenderSettings.profile`
(`RenderProfilePreset`: Mobile Low/High, Desktop Low/High/Ultra, Deck). The golden
harness pins `FrameQuality::LAB` and does not follow the desktop-high preset.

## What the profile turns on

- `render_scale` below 1 renders the HDR target smaller. It is upscaled by TAAU when
  anti-aliasing is TAA (Desktop High: native, or 0.67 as an opt-in performance budget; Deck: 0.7) and by a bilinear resolve
  otherwise (Desktop Low). Ultra stays at 1.0. Scale is ignored when both the post
  stack and TAAU are off, so a raw MSAA frame stays at the output size.
- The opt-in 0.67 Desktop High budget (`render_scale` 0.67 with a 2048² cascade)
  gathers screen-space GI at half resolution and joint-bilateral-upsamples
  it, uses 6+8 PCSS taps, and skips full-resolution SSR. Stock Desktop High (native
  scale) keeps the full-resolution SSGI gather, 12+16 taps and the full-resolution linear SSR march when stochastic SSR is not capable (Desktop High and Ultra normally use stochastic SSR, cutoff 0.55).
  A held camera (same unjittered view, sun, and draws) reuses the previous SSGI,
  SSR, and AO. Ultra's SSGI stays full resolution. SSAO on Desktop High is already
  half resolution and is sampled directly. Sun PCSS uses 12+16 taps except in the
  0.67 budget (6+8).
- `gpu_driven` (Desktop Low, Desktop High, Ultra and Deck) runs a compute frustum plus a Hi-Z test and
  draws with indirect commands. Shadow and depth-prepass draws of those meshes are
  instanced from one object buffer. Skinned color and blended materials stay on
  the CPU path. If the device cannot do indirect draws, the CPU loop is used.
- `ssao` darkens indirect light only. Off is a 1×1 white texture, which matches the
  lab goldens.
- `shadow_filter` selects hardware PCF, 3×3, 5×5, or a small PCSS kernel. Desktop
  High and Ultra use PCSS-lite.
- `max_lights_per_cluster` is the froxel cap. `max_reflection_probes` caps how many
  captured probes are blended into SSR misses. See [high-end lighting](high-end-lighting.md).
- `horizontal_plus` keeps the 16:9 vertical FOV on wider aspects and the 16:9
  horizontal FOV on narrower ones. The lab leaves it off.
- Deck is 1280×800, `render_scale` 0.7, TAAU, and a 40 fps cap. The cap is applied
  only when pacing has no cap yet. It does not clear a cap you already set, and it
  does not replace a variable-refresh present mode.

## Lights and debug views

A frame draws up to four directional lights (the sun with its cascades and three unshadowed ones),
`AmbientVolume` fill boxes, and every `DebugViewMode`. See [Render lights](render-lights.md) and
`render.lights`.

## Output

`OutputSettings` (a resource):

- `present`: `Vsync` (the default; same mapping as before), `Uncapped`, or `Vrr`
  (mailbox, else relaxed FIFO, else FIFO). On DX12, `Vrr` takes `Immediate` when the
  surface lists it, even if mailbox is listed too. See [swapchain](swapchain.md).
- `hdr`: `Sdr` (default), `Hdr10`, or `ScRgb`. The swapchain changes only when the
  surface lists that format and color space. See ADR-021: the image is still the
  AgX 0..1 grade.
- `frame_cap_fps`: `None` leaves `FramePacing` alone.

## Timings

`FrameRenderer::set_gpu_timing(true)` then `gpu_timings()` reports every pass that ran,
by name (`GpuTimings::report` prints every pass, zeros included): `shadows`, `clusters`,
`gi probes` (the GPU probe update, `gi_ms`), `prepass`, `cull`, `ssao`, `contact shadows`,
`forward`, `ssgi`, `ssr`, `atmosphere`, `clouds`, `fog`, `taau`, `scene total`, `post` and
`frame`. A skipped pass reads 0.00 ms and its scalar field stays 0. `gi_cpu_ms()` is only the
host-set CPU trace time, not a GPU pass. `render_lab` and `sim_bridge --bench-frames` print the same
named list (render_lab also prints the CPU trace). The Sponza bench uses the desktop-high profile, so paging,
probes, screen-space GI and SSR run there when the depth prepass is on.
`spawn_lab_scene` itself stays on the lab baseline, which leaves all of those off.
