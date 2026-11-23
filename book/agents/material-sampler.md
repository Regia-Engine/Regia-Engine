# Material sampler calibration

Material textures (albedo, normal, ORM, emissive, the texture table) are sampled through one
trilinear 8x anisotropic sampler. Vulkan leaves the anisotropic filter's scheme to the driver; only
the isotropic trilinear filter is defined exactly. Drivers narrow the blend between two mips once
anisotropy is on, and not in the same place. For a level fraction `f` an isotropic footprint
filtered at:

| Adapter | Isotropic sampler | 8x / 16x sampler |
|---|---|---|
| NVIDIA (RTX 5090, 615.71) | `f` | `clamp(2 f, 0, 1)` (never sharper than exact) |
| Mesa RADV (RX 7900 XTX, Mesa 26.2.6; Steam Deck) | `f` | `clamp(2 f - 0.5, 0, 1)` (sharper below `f = 0.5`) |
| ARM Mali-G715 (Pixel 9 Pro, driver r54p3) | `f` | `clamp(2 f - 0.5, 0, 1)` (RADV's curve exactly) |
| Apple M5 (Metal) | `f` | `f` (within filter-weight rounding) |
| lavapipe | `f` | `f` (within filter-weight rounding) |

The look and the TSR texture-bias tests were tuned on the first row. So when a renderer builds its
pipelines on a device, it runs a one-dispatch probe: through the material sampler's descriptor and
an isotropic one, it samples a texture whose mip `k` holds `k`, at quarter-mip fractions. The largest
amount by which the anisotropic sampler reads sharper than the isotropic one becomes a mip offset.
Every material sample through that sampler adds the offset to the frame's texture bias: the forward
and visibility-buffer paths, the texture table, the alpha-masked visibility prepass, hashed-alpha
minification and the transmission pass (ADR-5670). RADV and Mali measure 0.25, which is NVIDIA's curve exactly.
NVIDIA, Apple and lavapipe measure 0 and draw exactly as before. The centered narrow blend is therefore not one
vendor's or one driver's trait (an open AMD driver and ARM's own driver share it), which is why the offset is
measured and not looked up by adapter name (ADR-5797). The probe runs once per device and anisotropy clamp
and never touches frame counters. On the web it does not run (offset 0).

## Agent surface

- `render.get_capabilities` reports `material_anisotropy_lod_offset`: the offset in mips of the most
  recently calibrated device (null before any renderer built its pipelines).
- `REGIA_MATERIAL_ANISO_LOD_OFFSET=<mips>` (clamped to 0..=0.5) replaces the measurement; `0` turns
  the calibration off, which reproduces the uncalibrated frames on any adapter.
- `FrameRenderer::material_anisotropy_lod_offset()` returns the renderer's offset, and
  `regia_render::material_anisotropy_lod_offset(device, queue, clamp)` runs the probe. Both are for tests and tools.
  `material_aniso_calibration::measured_anisotropy_lod_offset(device, queue, clamp, ProbeTexture::D2 | D2Array)`
  is the measurement alone (no environment override, `None` where the probe cannot run); the terrain uses
  the array form.

## Verify on a card

```sh
cargo test -p regia_render --test material_sampler_lod -- --nocapture
cargo test -p lookdev --test tsr_motion tsr_at_067_keeps_native_texture_detail -- --nocapture
```

`the_material_sampler_is_never_sharper_than_trilinear` fails when the calibrated 8x sampler filters
an isotropic footprint sharper than the exact trilinear level, with and without the TSR bias.
`material_sampler_lod_dump` prints the raw curves of the isotropic, 8x and 16x samplers.

## Known limits

- The shadow pass's alpha-tested casters and the mobile forward path sample without the frame
  bias, so they are not calibrated. On a Mali phone at a Mobile tier that means opaque and masked
  standard materials (the lite forward path) filter as before, a quarter mip sharper than the
  blended, transmissive, graph and mask-decal draws of the same frame (ADR-5797). The terrain's
  sampler path takes its own offset from the same probe over a 2D array (ADR-5459, ADR-5797;
  `book/agents/terrain-runtime.md`).
- The point-filtered table sampler (`MaterialSampler::PointClamp`) takes the same bias as the
  others, although it is not anisotropic and needs none: on an adapter with a non-zero offset
  (RADV, 0.25) it switches to each coarser mip a quarter level sooner than on an adapter that
  measures 0. Nothing in the engine or its samples selects that sampler today.

## Comparing frames across adapters

Two more adapter differences show up when an agent compares pixels from two cards (ADR-5713). Neither
is corrected, because both are within what the graphics API allows:

- **sRGB decode.** `cargo test -p regia_render --test srgb_decode_probe -- --nocapture` prints how the
  adapter decodes each 8-bit sRGB code. An RX 7900 XTX, a Steam Deck and an RTX 5080 all return linear
  values with 8 significant bits (within 0.4 % of the exact curve from code 16 up, 0.54 % at most
  below), and AMD and NVIDIA round 29 of the 256 codes to different neighbors, 0.4 to 0.7 % apart;
  lavapipe returns a fitted curve that is up to 2.5 % off in the darkest codes. A Mali-G715 and an
  Apple M5 return the same table as each other, bit for bit: the exact curve rounded to twelve bits
  (`n / 4095`; within 0.03 of an 8-bit step everywhere, 1.25 % off at code 19 and 20 % at code 1). A value that depends
  on a single texel (a 1×1 mip used as a mean, a flat color swatch) differs between adapters by that
  much. Compare such frames with a tolerance, or measure a ratio inside one frame.
- **8-bit output.** Two adapters render the same frame to within single 8-bit steps, not bit for
  bit. A measure that sums small signed differences over a region (a ghosting trail, a variance) can
  move by a few percent from those steps alone.

`REGIA_DIAG_OUT=<dir>` makes the sRGB probe write its table as CSV, which is the quickest way to see
which codes two adapters disagree on.

A still frame is repeatable on one adapter: `cargo test -p regia_render --test ssr_linear_repeatable`
holds that for the linear reflection march, which an RX 7900 XTX rendered as two alternating images
before ADR-5713. `cargo test -p regia_render --test loop_break_fetch_probe -- --nocapture` prints
whether an adapter fetches a texel repeatably on the path that breaks out of a loop (the construct
that march used to have); no shader of the engine has it now.
