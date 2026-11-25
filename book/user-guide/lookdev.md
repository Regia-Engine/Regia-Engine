# Look-dev harness

<!-- index: Look-dev -->
<!-- index: Shot list -->

`samples/lookdev` is the measured feedback loop for the DO27 alpha maps. It builds two
procedural stand-ins in the terrain data shape, renders a fixed shot list offscreen, and
writes `shots.json`. After a render or terrain merge, `lookdev --compare` prints the deltas.

## Stand-ins

Europa is Bore Twelve: snow, wind-packed snow, blue ice, dirty ice, frosted basalt, regolith.
Mars is The Switchbacks: sandstone strata, scree, dust, gravel, bedrock, dune sand.

Each scene carries:

- a 16-bit eroded heightfield (baseline edge 4096, world width 4096 m)
- six material layers, each with an authored height map
- a macro-variation map (evidence edge 2048; `--macro 8192` is the 8K target and a CPU test
  checks that 8192 allocates). The evidence run stays at 2048: the terrain shader does not
  sample the map yet, and an 8192² `f32` image is 256 MiB before row padding
- a road trench with a shoulder lip, a tunnel mouth (heightfield notch plus the portal mesh
  drawn by the harness), and rock scatter. Scatter is still the terrain pyramid mesh until
  a rock mesh exists

Thermal erosion follows Musgrave, Kolb and Mace, SIGGRAPH 1989. Hydraulic erosion follows
the particle form of Mei, Decaudin and Hu, Pacific Graphics 2007. Both rows are in
`docs/provenance.md`.

## Shot list

Eight cameras per planet: close (1 m), mid (50 m), far (2 km), low sun, noon, tunnel mouth,
road bend, sky-heavy. Three profiles: Ultra (`DesktopUltra`), High (`DesktopHigh`), and
Deck-Medium. Deck-Medium selects the engine preset `Deck` (the profile table has one Deck
row). That is 48 shots. The list does not include the VeryLow tier or the offline Cinematic
profile. Render those with `RenderProfilePreset` (Cinematic through
`regia_render::cinematic::gate_preset`) when the comparison needs them. Each shot renders 30
frames so temporal stability has a real window.

```sh
cargo run -p lookdev --release -- --shots /tmp/lookdev \
  --width 1920 --height 1080 --heightfield 4096 --macro 2048 \
  --frames 30 --png-width 960
```

`--planet europa` or `--planet mars` restricts the list. With no arguments the binary prints
help and exits 1.

## What a frame contains

The heightfield is drawn through the terrain hook that is on main: one sun cascade, exponential
height fog, CDLOD, slope and altitude layers, scatter, and the tunnel hole. The harness also
draws `MeshTunnel::render_mesh` with the same EV100 / AgX lighting, because the hook punches
the hole and does not submit the mesh. Desktop High and Ultra request GTAO, PCSS-lite, four
cascades, the probe budget, and TAA. `shots.json` records those requests next to
`applied_terrain_cascades: 1`. Detecting SSR or GI does not mean those passes ran.

`build.rs` sets a cfg when a named item appears. On this tree that is `fn ssr_roughness_weight`,
`struct ReflectionProbe`, `struct GiField`, and `struct GpuGiGrid`. Atmosphere, clouds,
streaming, and macro sampling stay on their own needles (`struct AtmosphereModel`,
`struct VolumetricClouds`, `struct TerrainStreamer`, `struct MacroVariation`). The flag lands
in `shots.json`, so `--compare` shows that the lane merged. A new draw API still needs a
harness edit before its pixels are in the shot. The harness uploads each layer height map and
the macro map as `R32Float` and records the byte counts. The terrain shader samples them only
when the macro-lane probe flips. Behavior that arrives through `TerrainFrame`, `LayerDesc`,
fog, or CDLOD is picked up on the next build, because the frame is built with
`TerrainFrame::from_camera` and the layers start from `LayerDesc::ground`.

## Measurements

`shots.json` (version 1) stores, per shot, mean BT.709 luminance, contrast (luminance standard
deviation), percent near-black (Y < 8) and blown (Y > 250), saturation, the strongest
secondary autocorrelation peak after the lag-0 lobe (a smooth gradient is 0), RMS Laplacian
sharpness, temporal stability over the frame count, mean luminance of the bottom 45% of the
frame (`ground_luminance`), and mean GPU milliseconds for shadow, sky, terrain, scatter, and
water. Noon and low sun are tuned so Europa snow stays under the AgX shoulder and a low sun
still lights the ground.

Numbers come from the full framebuffer. The PNG is the last frame, box-downscaled to
`--png-width` when the frame is wider. Deck's render scale produces a smaller framebuffer, so
those PNGs stay at their native size when that size is already under the cap.

GPU times need `TIMESTAMP_QUERY` and `TIMESTAMP_QUERY_INSIDE_ENCODERS`. Otherwise every pass
is 0 and `gpu_timing` is false. A failed timestamp read drops timing and keeps the image.

```sh
cargo run -p lookdev -- --compare old/shots.json new/shots.json
```

The table is `new − old` for luminance, contrast, clipping, saturation, tiling, sharpness,
stability, ground luminance (`d_gnd`), and total GPU time, plus a line for each feature flag
or upload byte-count that changed.

## Default-look shots

`cargo run --release -p lookdev --bin default_look -- --out <dir>` renders four DO27-like
frames through the mesh renderer with nothing but the runtime defaults: props on snow, a
vehicle in the air, an enclosed cockpit, and a wide vista. `--fixed-ev <ev100>` pins the
exposure, `--side-by-side <before> <after> <out>` builds comparison images, and `LOOK_DEBUG_*`
environment variables switch single features off for A/B reviews (listed in
`lookdev::default_look`). The 320×180 lavapipe goldens are `samples/lookdev/goldens/`. See
[The default look](default-look.md).
