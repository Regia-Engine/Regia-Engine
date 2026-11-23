# Lighting option: Balanced or Depth

Players choose how indirect light (sky light and light bounced off the ground and walls) looks.
This choice is separate from the quality preset:

| Option | What lights a shadowed surface | Where |
|---|---|---|
| **Balanced** | The sky irradiance map with canyon occlusion, an estimate of light bounced off sunlit walls, and the mesh ground bounce (ADR-062/065/066). | Every platform. Default on Low, Medium, Deck and mobile. |
| **Depth** | GPU-traced irradiance probes: up to four camera-relative cascades of 20×10×20 probes, 2 m spacing ×3 per cascade, tracing the terrain heightfield and per-mesh distance fields. They gather sun, sky and emissive light over several bounces (emissive meshes of any size enter through an emitter list, not rays), and Chebyshev visibility keeps light from passing through closed walls. A half-resolution screen pass stores the probe light at the visible surfaces, and each pixel rebuilds it for its own normal. | Desktop only (not on Android). Default on Desktop High and Ultra. |

A map with a baked GI volume (`.fgiv`) uses it instead of the probes, under Depth and the preset
default alike. Only a chosen Balanced keeps the analytic terms. See [Baked GI volumes](baked-gi.md).
The renderer-side look `GiLook::Baked` (`render.set_gi_mode {"mode": "baked"}`) forces it and is not
a menu choice.

## Settings

- `GameSettings::graphics.lighting` is the value a game's options menu binds:
  `Some(LightingMode::Balanced)`, `Some(LightingMode::Depth)`, or `None` (the quality preset's
  default). It is applied live and persisted with the other graphics settings.
- `RenderSettings::gi_look` is the renderer-side value the settings plugin writes.
  `RenderSettings::effective_gi_look()` resolves `None` to the preset default.
- Preset defaults: Desktop High and Ultra ship Depth; Low, Medium, Deck and Android ship
  Balanced (ADR-133 records the cost and tracer numbers; the hull plate passes since round 4).
- Ground albedo: the probes' rays need the terrain's albedo where they hit it. A host calls
  `TerrainSharedDraw::bake_gi_albedo_map(device)` once at load, before the first frame. It
  bakes the albedo the terrain shader shades with, at 2 m texels. Without it the probes see one
  albedo for the whole map, and the bounce color and strength follow that average.

## Agent methods

- `render.get_gi_mode` returns `{ mode, probes, setting, preset_default, depth_available, persisted }`.
  `mode` is the look in effect (`"balanced"`, `"depth"` or `"baked"`). `setting` is the player's choice
  (`"default"` when unset). `graphics.lighting` is never `"baked"`: that look is renderer-side only.
- `render.set_gi_mode {"mode": "depth"}` sets the option and returns `{ before, after, note }`.
  `mode` is `"balanced"`, `"depth"`, `"baked"` or `"default"`; `"terms"` and `"probes"` are aliases of
  balanced and depth, and `"static"` is an alias of baked. Balanced and depth write
  `RenderSettings::gi_look` and, when the app has `GameSettings`, also `graphics.lighting`.
  `"baked"` sets `GiLook::Baked` and leaves `graphics.lighting` as it was. It applies on the next
  extracted frame; without an installed volume (`FrameRenderer::set_baked_gi`) the frame shades as
  balanced. On Android, `"depth"` and `"baked"` are refused.

Depth needs the post stack (a linear-HDR frame), so an LDR lab frame always uses Balanced. After
a switch or a camera cut the probes take about 30 frames to settle. To compare the two:

1. Call `play.pause` and `render.set_gi_mode {"mode": "balanced"}`, then `play.step 30` and
   `seeing.screenshot` (target `offscreen`).
2. Call `render.set_gi_mode {"mode": "depth"}`, then `play.step 30` and `seeing.screenshot`.
3. Judge the pair by eye against the reference plate. To check against ground truth, run
   `seeing.reference_trace` and `seeing.compare` with `aov: "indirect_diffuse"` (guide topic
   `renderer-excellence`).

## What to look for

- **Shadowed ground and walls:** Depth lights a shadowed trench floor only from the walls and
  sky the probes actually see. Where the terrain is open it is close to the path tracer.
  Balanced over-lights Mars canyon shadows about 2× against the tracer.
- **Interiors:** with Depth, a sealed room stays dark (under 1 % light leak).
- **Close-ups on open ground:** under the probes, a layer's ORM occlusion only shades
  crevices against its own texture mean. Texture AO is not geometry the probes trace (a scanned
  dune sand averages 0.36). The heightfield cavity and the baked macro AO do not darken the
  probe light either: the probes trace that relief themselves (ADR-169). Terrain debug views
  14 and 15 print the occlusion inputs: cavity, macro AO, probe coverage, ORM and SSAO.
- **Which meshes the probes trace:** rigid, opaque, shadow-casting meshes at least 1 m in
  radius whose distance-field voxel stays within 8 m (meshes up to ~320 m long). A thin plate
  (no thicker than 1 m: a floor, a pad, a studio ground) is traced at any length.
- **Known limits:** a receiver inside a shadow smaller than a probe cell (2 m near the camera)
  still takes some light from probes that see the sunlit ground beyond it, so small shaded
  pockets read brighter than the path tracer. Terrain more than ~450 m from the camera lies
  outside the cascades and keeps the analytic sky and bounce light. Europa's shaded trench flats
  read above the path tracer for the same sub-cell reason. The screen-space GI bounce is added on
  top of the probes' own bounce (about 3 % too bright in shade).
- **Cost:** GPU timings report it as `gi_ms`: the probe update plus the screen pass. On the
  RTX 5090 at 1440p, with the clocks up, the Mars plates cost about 0.26–0.28 ms per frame.
  For a bench run, set `LOOK_R2_PERF_HEAT=1000000` with `look_r2 --perf`. Without it, an idle
  card sits in its low power state and every pass reads about 3× slower.

Look-dev: `LOOK_R2_PROBES=1 cargo run -p lookdev --bin look_r2 -- --out <dir>` renders the DO27
plates with Depth. `docs/evidence/gi-probes/sheet/` holds the UE | Balanced | Depth sheet.
`pt_plates` (with `LOOK_R2_PROBES=1`) writes `<shot>_probe_vs_pt_irradiance.png`: probe
irradiance on the left, the tracer's on the right, at one exposure.

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
render.get_gi_mode {}            # { mode, probes, uses_bake, baked, setting, ... }
```
