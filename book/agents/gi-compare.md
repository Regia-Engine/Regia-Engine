# Baked-light look review (`cargo xtask gi-compare`)

`gi-compare` is the standard kit for judging baked light. For each camera in a views file it
renders the frame lit by the baked GI volume, the same frame with live GI (DDGI), and the path
tracer's reference. It shows each of them as the full image and as indirect light only, on one
contact sheet per view, with a false-colour error tile. It also writes a `summary.json` with
image scores, a leak metric and the volume's status.

The numbers point you at problems. They do not pass a bake. A bake is judged by looking at the
sheets, and the last line of every table says so.

The command drives the renderer and the path tracer in-process through their public APIs. It adds
no engine API and no agent method.

## Running it

```text
cargo xtask gi-compare --scene assets/fixtures/gi_compare/cornell.gi_compare.ron --spp 1024
cargo xtask gi-compare --read target/gi-compare/cornell/summary.json
```

| Option | Default | Meaning |
|---|---|---|
| `--scene <file>` | required | A fixture, `*.gi_compare.ron`. A level is refused for now (see *Limits*). |
| `--views <file>` | `<stem>.views.ron` next to the fixture | The cameras, the output size and the exposures. |
| `--out <dir>` | `target/gi-compare/<fixture name>` | Output directory. Files with the names below are replaced. |
| `--spp <n>` | 256 | Reference samples per pixel (1 to 65536). |
| `--volume <file.fgiv>` | bake in-process | Use an existing volume instead of baking the fixture's layout. |
| `--gpu-lock <file>` | none | Hold an exclusive lock on this file during the GPU work. It waits for the lock first. |
| `--reconstruction <name>` | `l1` | `l1` (the lookup's default), `zh3` (ADR-1377), `windowed` or `sweep`. This build has no `windowed` (see *Limits*). |
| `--ddgi-frames <n>` | 60 | Frames the DDGI frame renders so its probes settle. |
| `--baseline <summary.json>` | none | A previous run: a mean FLIP that rose by more than the tolerance is flagged. |
| `--flip-tolerance <x>` | 0.01 | FLIP increase that counts as a regression. |
| `--read <summary.json>` | none | Print the table and flags of an existing run. Needs no GPU. |
| `--size <W>x<H>` | the views file's | Render every view at this size instead, e.g. `1920x1080` for a review pack. The field of view is vertical, so a wider frame shows more to the sides. |
| `--emissive-scale <x>` | 1 | Scale of the bake's emissive component in the baked frame (`BakedGiSettings::emissive_scale`). A diagnostic: 0 leaves the emitters' light to the emissive area lights alone. |
| `--emissive-lights <n>` | Desktop High's 96 | Emissive area light budget of the raster frames. A diagnostic: 0 renders emitters as the tiers without area lights do (the bake alone carries their light), and the reference's indirect light then includes the emitters' first bounce. |
| `--flythrough-frames <n>` | 6 | Baked frames along the views file's `flythrough` path (see *Fly-through*). 0 skips it; a views file without a path renders none. |

The adapter is the headless default. On a machine with several GPUs, choose one with
`REGIA_GPU_UUID` or `REGIA_GPU_PCI` and `REGIA_GPU_STRICT=1`. Either wrap the command in
`flock <lock file>` or pass `--gpu-lock <lock file>`, not both on the same file: the two locks
would wait on each other forever.

Exit codes: 0 when nothing was flagged, 1 when something was, 2 when an input was refused or the
GPU work failed. A refused input prints `gi-compare: [code] message`.

## What each view renders

All frames use Desktop High (with `--emissive-lights` as its emissive area light budget when given) at the views file's size, with no temporal upscaling, no Hor+ (so the
field of view matches the tracer exactly) and no volumetric fog or clouds.

1. **Baked.** The volume is installed and the baked look is on, as `render.set_gi_mode {"mode":
   "baked"}` selects it. The fixture's capsule occluders are set.
2. **DDGI.** No volume, and DDGI is forced on (the `depth` look) for `--ddgi-frames` frames. This
   frame is context only and is never judged.
3. **Direct.** No volume, no DDGI, no SSGI and no flat ambient. This leaves the frame's analytic
   light: the sun, local lights, emitters seen directly, and on Desktop High the light of the
   area lights fitted to emissive meshes (`emissive_lights`).
4. **Reference.** `regia_pt` at `--spp`, tracing the engine's own extraction of the raster frame
   (`regia_pt::extract::from_frame`). Both renderers see the same meshes, materials and lights.

Indirect light is isolated the same way on both sides:

- **Raster:** `baked − direct` per channel, clamped at 0 (DDGI: `ddgi − direct`).
- **Reference:** the tracer's `indirect_diffuse + indirect_specular` AOVs. Desktop High lights
  the surroundings of emissive meshes with fitted area lights, so the direct frame already holds
  the emitters' first bounce and nothing is added (ADR-1326). On a tier without emissive area
  lights (`emissive_lights` 0), the raster lights nothing with an emitter and only the bake carries
  its light; there the reference adds the emitters' first bounce, `direct − direct of the same
  scene with emission off`, from a second, cheap trace (one bounce, a quarter of the spp, at
  least 16).

Note that with a volume installed, Desktop High lights the emitters' surroundings twice: once
with the area lights and once with the bake's emissive component, which also holds the
emitters' first bounce. The `cornell` sheets show it as a baked frame brighter than the
reference everywhere (v0.2.0 look review, ADR-1326).

## The contact sheet

`<view>.png` has three header lines and then two rows of four tiles:

| Row | Exposure | Tiles |
|---|---|---|
| Full | the view's `ev100` | baked, DDGI, reference, FLIP of baked against the reference |
| Indirect | the view's `indirect_ev100` | baked indirect, DDGI indirect, reference indirect, FLIP of the indirect pair |

A `--reconstruction sweep` adds a row with one baked tile per reconstruction.

The last row is the review row (ADR-1326), at the full row's exposure:

| Tile | Shows |
|---|---|
| direct only | The direct-only frame: what the view looks like with no indirect light at all. Everything the bake adds is the difference between this and the baked tile. |
| \|baked − ref\| ×4 | The absolute difference between the baked frame and the reference, per channel, times 4, through the same display transform. Black is a match. Unlike the FLIP tile it keeps the colour of the error: a red-tinted patch is red light that one of the two has and the other has not. It does not say which side is brighter; compare the two tiles for that. Reference noise shows as speckle here too, and at ×4 the edges of sun shadows saturate where the shadow map and the traced shadow differ by a texel: that is direct light, not GI, so read this tile next to the FLIP indirect tile. |

Every tile in a row has the same exposure and goes through the runtime's AgX display transform.
That makes brightness differences inside a row real. The FLIP tiles use the false-colour map
black → magenta → orange → white, where black means no visible difference.

How to read it:

- Compare **baked** with **reference** first, in both rows. Look for missing bounce in shadowed
  areas, light where the reference is dark (a leak), and wrong colour bleeding.
- The **indirect row** shows what the bake controls. A bright patch in the FLIP indirect tile
  that is dark in the FLIP full tile is a difference hidden by the direct light.
- **DDGI** shows what the frame would look like without the bake. If the bake is worse than DDGI
  somewhere, that is worth a note.
- The reference has noise at low spp. Fine speckle in the FLIP tiles that follows no edge or
  surface is the reference's noise, not the bake's error. Raise `--spp` before concluding
  anything from it.
- Sky pixels (where the reference hits nothing) are taken from the reference before scoring, so
  the scores judge surfaces only. The tiles show both backgrounds unchanged.

## Files

| File | Contents |
|---|---|
| `summary.json` | The run (schema below), rewritten after every view. |
| `<view>.png` | The contact sheet. |
| `<view>_{baked,ddgi,reference,error}.png` | The full-row tiles. |
| `<view>_{baked,ddgi,reference}_indirect.png`, `<view>_error_indirect.png` | The indirect-row tiles. |
| `<view>_reference.exr` | Every AOV of the reference trace, linear (cd/m²), with its metadata. |
| `<view>_raster.exr` | Layers `baked`, `ddgi`, `direct`, `baked_indirect`, `ddgi_indirect`, `reference_indirect`, linear. |
| `<view>_direct.png`, `<view>_diff4x.png` | The review row's tiles. |
| `flythrough_NN.png`, `flythrough.png` | The fly-through frames and their strip (only with a `flythrough` path). |
| `<fixture>.fgiv` | The volume baked in this run. Pass it back with `--volume` to rerun without baking. |

## `summary.json`

`schema` is 1. `--read` refuses any other schema (`summary_schema`).

```json
{
  "schema": 1,
  "fixture": "leak_room",
  "fixture_path": "assets/fixtures/gi_compare/leak_room.gi_compare.ron",
  "views_path": "assets/fixtures/gi_compare/leak_room.views.ron",
  "size": [480, 270],
  "spp": 1024,
  "ev100": 15.0,
  "indirect_ev100": 12.5,
  "reconstruction": "l1",
  "device": "NVIDIA GeForce RTX 5090 (Vulkan)",
  "bake": { "source": "baked", "probes": 3487, "valid": 3300, "inside_geometry": 120,
            "under_terrain": 0, "relocated": 410, "rejected_rays": 0, "seconds": 4.2 },
  "views": [
    {
      "name": "inside_door",
      "sheet": "inside_door.png",
      "ev100": 11.0,
      "indirect_ev100": 10.0,
      "full":     { "flip_mean": 0.08, "flip_p95": 0.22, "ssim": 0.91, "rel_rmse": 0.31,
                    "mean_reference": 210.0, "mean_test": 190.0 },
      "indirect": { "flip_mean": 0.11, "flip_p95": 0.30, "ssim": 0.85, "rel_rmse": 0.42,
                    "mean_reference": 60.0, "mean_test": 52.0 },
      "ddgi_full": { "flip_mean": 0.12, "flip_p95": 0.35, "ssim": 0.88, "rel_rmse": 0.40,
                     "mean_reference": 210.0, "mean_test": 230.0 },
      "reconstructions": [],
      "leak": { "pixels": 98000, "baked_mean": 41.0, "reference_mean": 44.0, "floor": 2.1,
                "leak": 0.0, "threshold": 0.25 },
      "gi": { "installed": true, "active": true, "probes": 3487, "gpu_bytes": 1048576,
              "error": null, "scales": [[1, 1, 1], [1, 1, 1], [0, 0, 0], [1, 1, 1]],
              "sun_mismatch_deg": 0.0, "occluders": 0, "occluder_cap": 64,
              "occluders_dropped": 0, "interior_bricks": 1, "emitters": 0,
              "emissive_lights": [0, 0] },
      "timings": { "raster_baked_ms": 40.0, "raster_ddgi_ms": 900.0, "raster_direct_ms": 35.0,
                   "reference_ms": 5200.0, "reference_direct_ms": 0.0, "compare_ms": 300.0 },
      "tiles": ["inside_door_baked.png", "inside_door_reference.exr"]
    }
  ],
  "total_seconds": 30.0
}
```

The values above are illustrative, not measured. In `gi`, `emitters` is the number of emitters the
volume's bake named (`null` for a `.fgiv` v1/v2 volume) and `emissive_lights` is the baked frame's
uploaded emissive area lights and how many of them the bake holds (ADR-1386); summaries written before
them read as `null` and `[0, 0]`.

- **Scores** (`full`, `indirect`, `ddgi_full`, each `reconstructions` entry) come from
  `regia_render::compare`: FLIP and SSIM on the pair display-encoded at the row's exposure, and
  RMSE of linear luminance. `rel_rmse` is the RMSE over the reference's mean luminance. It is
  `null` when the reference is black. Lower FLIP and higher SSIM are closer to the reference.
- **`leak`** is present for a view with an `interior` box, when some pixel of the reference hits
  inside that box grown by 5 cm. `baked_mean` and `reference_mean` are the mean indirect luminance
  over those pixels. `floor` is 1 % of the reference's mean luminance over every pixel that hits
  geometry. `leak = max(0, baked_mean − reference_mean) / max(reference_mean, floor)`. The value
  0.5 means the bake puts 50 % more indirect light in the room than the reference. A darker bake is
  never a leak. The floor keeps a sealed room's metric finite: there the reference is almost 0.
- **`gi`** is the renderer's baked GI status after the baked frame. It is the `baked` object of
  `render.get_gi_mode` (see [Baked GI volumes](baked-gi.md)) and comes from the same
  `FrameRenderer::baked_gi_status`.
- **`timings`** are wall times, recorded and never judged.

## Flags

`--read` (and the end of every run) prints one row per view and then the flags:

- a **leak** above the view's `leak_threshold` (default 0.25);
- a view the **bake did not light** (`gi.active` false), with the renderer's error;
- with `--baseline`, a **FLIP regression**: the mean FLIP (full or indirect) of a view rose by more
  than `--flip-tolerance` over the baseline view of the same name. A baseline at another spp or
  size is noted, because FLIP moves with noise and size.

The last line always reads: *These numbers are only a guide: look at the plates (one contact sheet
per view) before judging the bake.*

## Fixtures and views

The fixtures live in `assets/fixtures/gi_compare/`:

| Fixture | Checks |
|---|---|
| `cornell` | A 2 m Cornell-like box lit only by a 0.4 m overhead emitter: the L1 case of ADR-206 (low under the source, ringing on the far side). |
| `leak_room` | A closed 6 × 3 × 6 m room with one doorway on sunlit ground. Its interior views carry the room's air as the leak mask. |
| `rocks_capsule` | Rolling terrain, three rocks in the bake and one dynamic capsule left out of it. The capsule is a capsule occluder over the volume (ADR-282). |
| `corridor` | A 24 m corridor open at one end, with three windows in its east wall and an ochre west wall: light carried down a long interior; the far end gets bounce only. |
| `window_room` | A closed 6 × 3 × 5 m room lit through one window: the sun patch on the wooden floor, the sky through the window, a red wall for colour bleeding. |
| `courtyard` | A 16 m courtyard between four 8 m blocks: the sunlit sandstone facade bounces into the shadowed half of an open yard. No interior flags. |
| `outpost_standin` | A 256 m valley with a hab and a hangar (interiors, doors facing the sun), a tower, containers and rocks: a large outdoor map with interiors, as a stand-in for a game map. No game data. |

The last four, and the `flythrough` paths, were added for the v0.2.0 look review (ADR-1326). Their bakes use
`regia bake-gi`'s default trace settings (256 rays × 16 samples, 8 bounces).

A fixture (`format_version: 1`) is text. The geometry is generated in code from it: boxes,
ellipsoids, vertical capsules, and a terrain grid summed from plane waves. It names materials
(albedo, roughness, metallic, emissive in cd/m²), a sun (`toward`, `color`, `lux`) and a flat
sky (`sky_lux`: the raster's flat ambient, and a uniform sky of radiance `sky_lux / π` in the
tracer). It also names the probe layout (terrain layer, bricks, interior boxes) and the bake
settings. A shape with `baked: false` is a dynamic unit: drawn and traced, but not baked. A
dynamic capsule with `occluder: true` is also set as a capsule occluder, at 0.9 of its radius so
it fits inside the hull.

```ron
(
    format_version: 1,
    name: "small",
    sun: Some((toward: (0.3, 0.8, 0.2), lux: 50000.0)),
    sky_lux: 10000.0,
    materials: [(name: "ground", albedo: (0.3, 0.3, 0.3))],
    terrain: Some((size: (32.0, 32.0), spacing: 1.0, material: "ground",
        waves: [(amplitude: 0.5, wavelength: (8.0, 0.0))])),
    shapes: [
        (name: "rock", kind: Ellipsoid(radii: (1.0, 0.6, 0.8)), material: "ground", at: (2.0, 0.3, 0.0)),
    ],
    layout: (terrain: Some((spacing: (4.0, 4.0), levels: [0.5, 2.0]))),
)
```

A views file (`format_version: 1`) gives the size, the default exposures and the cameras. A view
may set its own `ev100` and `indirect_ev100`, for example an interior next to a sunlit exterior.
`indirect_ev100` defaults to `ev100 − 2`.

```ron
(
    format_version: 1,
    size: (480, 270),
    ev100: 15.0,
    indirect_ev100: Some(12.5),
    views: [
        (
            name: "inside_door",
            eye: (0.0, 1.6, -2.2),
            target: (0.0, 1.0, 2.9),
            fov_y_deg: 75.0,
            interior: Some((min: (-3.0, 0.0, -3.0), max: (3.0, 3.0, 3.0))),
            leak_threshold: Some(0.25),
            ev100: Some(11.0),
        ),
    ],
)
```

## Fly-through

A views file may list a `flythrough` path: two or more camera keys. With `--flythrough-frames n` (default 6),
gi-compare renders `n` baked frames spaced evenly by the eye's distance along the path through the keys, after the
views. It does not trace a reference for them: the strip is for looking at the baked light in motion (flicker
between frames, a brick seam the camera crosses, an interior that pops as the camera enters it), not for scores.

```ron
flythrough: [
    (eye: (0.0, 1.6, 10.0), target: (0.0, 1.5, 2.5), fov_y_deg: 55.0, ev100: Some(14.5)),
    (eye: (0.0, 1.6, 0.8), target: (-1.0, 0.8, -2.5), fov_y_deg: 70.0, ev100: Some(12.0)),
],
```

Between two keys, the eye, target, field of view and exposure are interpolated linearly. A key without `ev100`
uses the file's. A path whose eye never moves (a pan) is spaced by key instead. The path is not checked against
the geometry: choose keys so that no frame lands inside a wall.

## Error codes

Codes are lowercase ASCII, digits and `_`, printed in brackets.

| Code | When |
|---|---|
| `views_parse` | The views file is not RON or not the expected shape (unknown field, wrong type). |
| `views_version` | `format_version` is not 1. |
| `views_size` | A side of `size` (or of `--size`) is 0 or more than 4096, or `--size` is not `<W>x<H>`. |
| `views_exposure` | An `ev100` or `indirect_ev100` is not finite. |
| `views_empty` | No view. |
| `view_name` | A name is empty or uses characters other than `a-z`, `0-9`, `_` and `-`. |
| `view_duplicate` | Two views share a name. |
| `view_camera` | Eye or target is not finite, they coincide, or the camera looks straight up or down. |
| `view_fov` | `fov_y_deg` is not between 1 and 170. |
| `view_interior` | The interior box is empty, inverted or not finite. |
| `view_leak_threshold` | The leak threshold is negative or not finite. |
| `emissive_scale_range` | `--emissive-scale` is negative or not finite. |
| `views_flythrough` | The `flythrough` path has one key, a key has a bad camera, field of view or exposure, or the path looks straight up or down between two keys. |
| `fixture_parse` | The fixture is not RON or not the expected shape. |
| `fixture_version` | `format_version` is not 1. |
| `fixture_material` | A shape or the terrain names an unknown material, two materials share a name, or a colour is negative. |
| `fixture_shape` | A shape's size is not positive and finite, or an occluder is not a dynamic capsule. |
| `fixture_terrain` | The terrain's size and spacing give fewer than 2 or more than 1025 samples per side. |
| `fixture_layout` | No probes, a terrain layer without a terrain, a bad brick, or a bad `moment_resolution` / `max_distance`. |
| `fixture_light` | A sun or sky value is negative or not finite. |
| `fixture_unreadable`, `views_unreadable`, `summary_unreadable` | The file could not be read. |
| `summary_parse` | `--read` or `--baseline` got a file that is not a gi-compare summary. |
| `summary_schema` | The summary has another schema. |
| `scene_missing` | Neither `--scene` nor `--read` was given. |
| `scene_level_unsupported` | `--scene` names a level (`*.level.ron` or `*.scene.ron`). |
| `spp_range` | `--spp` is 0 or more than 65536. |
| `reconstruction_unknown` | `--reconstruction` is not `l1`, `windowed`, `zh3` or `sweep`. |
| `reconstruction_unavailable` | The reconstruction is not in this build's baked lookup (`windowed`, and so `sweep`). |
| `gpu_lock` | The `--gpu-lock` file could not be opened or locked. |
| `gpu_unavailable` | No headless adapter, or the tracer did not compile. |
| `volume_unreadable` | The `--volume` file is missing or not a readable `.fgiv`. |
| `bake_failed` | The in-process bake failed. |
| `render_failed` | A raster frame or its linear readback failed. |
| `trace_failed` | A reference trace failed. |
| `out_unwritable` | An output file could not be written. |

## Limits

- **Levels are not supported yet.** Isolating indirect light needs the raster frame's linear
  HDR. The renderer has it (`FrameRenderer::request_hdr_capture`), but the agent protocol has no
  method that returns it, and a level runs in an app. A level route needs a linear capture method
  on the agent protocol (an `hdr` option on `seeing.screenshot` that writes an EXR). ADR-314 has
  the plan.
- **Reconstructions.** The baked lookup evaluates its SH as `l1` (the default) or `zh3` (ADR-1377,
  `render.set_gi_sampling {zh3: true}`, guide page [gi-sampling](gi-sampling.md)); compare the
  two with one run each and `--baseline`. It has no windowed L1: `--reconstruction windowed` and
  `sweep` are parsed, then refused with `reconstruction_unavailable` before any GPU work. The
  sweep's sheet row and summary entries are already wired.
- **The direct-only frame** relies on the lab frame having no outdoor parameters, so no analytic
  bounce term runs in it. Check the `direct` layer of `<view>_raster.exr` once per new renderer
  release: shadowed areas must be black.
- Fixtures have no local lights. Point and spot lights are live, not baked (ADR-206), so their
  bounce would be missing from the baked frame by design.
