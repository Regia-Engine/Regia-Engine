# Mobile forward path

Phones and other tile GPUs cannot carry the full forward shader: it costs 0.15-0.39 us per pixel on
a Mali-G78 and an Adreno 643, spread over every lighting feature. Both Mobile presets therefore ask
for a separate small fragment shader, `shaders/forward_mobile.wgsl`, and it applies on tile GPUs
only. A Mobile preset on a desktop GPU or a software rasterizer keeps the full shader and its pixels.

| Tier | Sun shadow | Local lights | Used by |
|---|---|---|---|
| `off` | full shader | full shader | every Desktop preset, Deck, Cinematic; desktop adapters at any preset |
| `low` | 1 cascade reaching 40 m, one hardware 2x2 tap | 4 per cluster, no shadows | Mobile Low on a tile GPU |
| `high` | 2 cascades, four taps | 8 per cluster, one tap per shadow | Mobile High on a tile GPU |

The lite shader draws opaque and masked standard materials. Blended, transmissive, material-graph and
mask-decal draws keep their own pipelines, so those still cost what they cost. A lightmap receiver draws
the lite shader with its baked light: one bilinear fetch at `low`, a second one for the direction at
`high` ([lightmaps](lightmaps.md)).

What a lite frame gives up: skin, eye, hair, sheen, clearcoat and anisotropy shade as the standard
surface; detail maps, wear and parallax are ignored; no specular antialiasing; no multi-scatter
compensation on the sky reflection (lights keep the material's since ADR-4930, a lightmap receiver's
baked reflection since ADR-5945); the occlusion map as it is on the sky light (a lightmap receiver's
baked diffuse takes the multi-bounce form since ADR-5945); no SSAO, contact shadows, irradiance
probes, baked GI volumes, mesh occluders, ground bounce or translucent shadows. Sky reflection and
irradiance stay (one fetch each).

Rain and snow on surfaces stay (spec 99): a frame that is wet, snowy or has a precipitation
extinction draws a weather variant of the lite shader (`LITE_SURFACE_WEATHER`), composed the first
time such a frame draws; a dry frame keeps the plain one. The High tier keeps covered surfaces dry
through the occlusion view; the Low tier has none. See
[Rain, snow and wet surfaces](weather-precipitation.md#surface-weather).

## Checking which shader a frame draws

```sh
regia agent call render.get_forward_lite '{}' --project <project>
```

Returns `{ tier, profile, override }`: `tier` is what the frame draws, `profile` what the preset asks
for, `override` what an agent or `REGIA_FORWARD_LITE` forced (`null` when neither did). The agent's
tier wins when both are set. The environment tier is the one recorded when the device was created.

## Comparing the two shaders on a phone

```sh
regia agent call render.set_forward_lite '{"tier": "off"}' --project <project>   # the full shader
regia agent call render.set_forward_lite '{"tier": "auto"}' --project <project>  # back to the preset
```

`tier` is `auto`, `off`, `low` or `high`. It applies from the next frame and is not saved. An unknown
tier returns `invalid_params`. `render.graph` lists each composed lite shader under the forward
permutations (`FORWARD_LITE_LOW` or `FORWARD_LITE_HIGH`, at most 8 sets per device, and 8 more
with `LITE_SURFACE_WEATHER` once a frame is wet).

`REGIA_FORWARD_LITE=auto|off|low|high` sets the same override for a whole process, which lets a
desktop GPU draw the lite shader.

## Measuring without an APK

`mobile_forward_bench` (package `lookdev`) renders a default-look shot headless and prints per-pass
GPU times, so it runs from `/data/local/tmp` over adb. `--stage-dir <dir>` stages edited `.wgsl`
files, so a shader change reaches the phone without a rebuild:

```sh
adb shell 'cd /data/local/tmp/regia-test && ./mobile_forward_bench --out out --preset MobileLow --lite auto --width 1280 --height 720'
```

Keep pushed files under `/data/local/tmp/regia-test/` and delete them afterwards.
