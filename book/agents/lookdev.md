# Look-dev compare

After a photoreal merge, render the shot list and diff it against the committed baseline.
The lead reads the `--compare` table. `docs/evidence/lookdev/baseline/` is the RTX 5080
capture (960-wide PNGs and `shots.json`). `cargo test -p lookdev` is the lavapipe check;
it does not bless those PNGs.

## Render

Headless. No window. Lavapipe is the test device; the evidence run pins the NVIDIA adapter:

```sh
flock /tmp/regia-gpu.lock flock /tmp/ferrite-gpu.lock env VK_LOADER_DRIVERS_SELECT='*nvidia*' \
  cargo run -p lookdev --release -- --shots /tmp/lookdev-nvidia \
  --width 1920 --height 1080 --heightfield 4096 --macro 2048 \
  --frames 30 --png-width 960
```

Tests use `LookdevConfig::tiny()` (48² heightfield, 64×36, both planets, 30 frames) through
`RenderDevice::headless()`. That is `cargo test -p lookdev`.

## Compare

```sh
cargo run -p lookdev -- --compare docs/evidence/lookdev/baseline /tmp/lookdev-nvidia
```

A directory means `shots.json` inside it. The first lines are the two adapters and any
feature flag that changed (`ssr_in_linked_crates`, `volumetric_atmosphere_in_linked_crates`,
`gpu_samples_macro_variation`, and the rest on `EngineFeatures`). Then one row per shot id
`{planet}/{camera}/{profile}`.

Read a row as `new − old`:

- `d_lum`, `d_con` — mean luminance and contrast on the 8-bit BT.709 plane
- `d_black`, `d_blow` — percentage-point change in near-black and blown pixels
- `d_sat`, `d_tile`, `d_sharp` — saturation, secondary tiling peak, Laplacian sharpness
- `d_stab` — temporal stability over the 30 frames. A static camera sits at 1, including a
  frozen blown or black frame, so stability is not a lighting check
- `d_gnd` — mean luminance of the bottom 45% (the ground band on horizon cameras)
- `d_gpu` — total GPU milliseconds (0 on a device without timestamp queries)

`missing_in_new` / `missing_in_old` list shot ids that do not pair. A profile rename shows
up there.

## What the numbers are measuring

The image is the terrain hook: sun cascade, height fog, CDLOD, six layers, scatter, hole,
plus the portal mesh the harness draws itself. `applied_terrain_cascades` stays 1.
`ssr_in_linked_crates` and `gi_probes_in_linked_crates` are true on this tree because
`fn ssr_roughness_weight`, `struct ReflectionProbe`, `struct GiField`, and `struct GpuGiGrid`
are linked. That is presence, not a second cascade and not an SSR or GI pass.
Ultra / High / Deck-Medium still change CDLOD range, fog density, scatter budget, and (Deck)
framebuffer scale. Deck-Medium is `RenderProfilePreset::Deck`.

Layer height maps and the macro map are authored and uploaded (`uploaded_*` and the byte
counts). `gpu_samples_macro_variation` flips when the terrain-photoreal symbols land; until
then the shader blends by slope and altitude only. Evidence macro is 2048 for that reason
and for the 10 minute GPU window. `--macro 8192` still allocates; the CPU test covers 8192
without rendering it. Rock scatter stays the terrain pyramid mesh.
