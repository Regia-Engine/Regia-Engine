# Lens effects: flare, dirt, chromatic aberration, starburst (agents)

The lens effects pass draws four camera-lens artefacts over the scene-referred HDR image, at the
frame's `before_post` slot (the frame graph lists it as the registered pass `lens_effects`, see
[Frame graph and host passes](render-graph.md)), before exposure, bloom and the tonemap
(ADR-1863):

- **Ghost flare.** A built-in synthetic lens is traced on the CPU (the reference in
  `regia_render::post_ref::lens_flare`): its most visible two-bounce ghosts, on both sides of the frame
  centre, become a table of colour (traced energy per red, green and blue wavelength), magnification
  `m` and footprint radius. Every pixel brighter than `flare_threshold` is a light; a light at
  centred frame position `c` puts each ghost at `m · c`, so ghosts line up through the frame centre
  and move against the light when `m` is negative. A ghost is an image of the six-bladed iris, with
  a real edge; a wide bright area casts a softer, larger ghost.
- **Lens dirt.** A procedural dust-and-smudge mask lit by the flare and by a wide blur of the
  bright image.
- **Chromatic aberration.** Lateral colour: red is magnified and blue minified about the frame
  centre by `chromatic_aberration` (a fraction of the radius); green stays on its pixel.
- **Starburst** (ADR-3196). Diffraction by the iris blades: each bright pixel is smeared along six
  directions, one per blade of the six-bladed iris, falling off with distance. It reads the same
  bright image as the dirt, so a lamp or sun shows thin spikes; `starburst` is the strength (0–10,
  0 off).

## Opt-in

Every strength is 0 by default, and a view whose strengths are all 0 records nothing (the frame is
bit-identical to one without the pass). The strengths of a view come from, in order:

1. the project's `PostSettings::lens_effects` (`RenderSettings`),
2. a `LensEffects` component on the camera, which replaces the project's for that camera,
3. post volumes at the camera's eye (`lens_flare_intensity`, `lens_flare_threshold`,
   `lens_dirt_intensity`, `chromatic_aberration` overrides (the starburst and `after_dof` are camera-only), [post volumes](post-volumes.md)),
4. the quality preset's gate: `RenderProfile::lens_effects` is on for Desktop High, Desktop Ultra
   and the cinematic profile only; elsewhere the effects are off whatever was asked.

## Methods

| Method | Permission | What it does |
|---|---|---|
| `render.lens_effects { entity?, light? }` | read | A camera's lens effects, what its view resolves to, the pass's counters, the ghost table and, for a light position, where the ghosts land. |
| `render.set_lens_effects { entity?, ... }` | edit scene | Sets a camera's `LensEffects` (inserting it), or removes it. |

`entity` defaults to the active window camera (the window camera with the lowest `order`).

### `render.lens_effects`

Params: `entity` (default the active window camera) and `light`, an optional `[x, y]` centred frame position
of a light (x right, y down; −1..1 inside the frame, up to ±4 beyond it).

Result:

- `entity`: the camera.
- `has_component`, `component`: the camera's `LensEffects` (`null` when it has none).
- `requested`: the strengths after the project, the camera and the post volumes; `effective`: the
  same after the preset's gate. Each holds `flare_intensity`, `flare_threshold`, `coating`,
  `dirt_intensity`, `chromatic_aberration`, `starburst`, `after_dof` and the booleans `active`,
  `flare`, `dirt`, `aberration`, `starburst_active`.
- `preset_allows`: `RenderProfile::lens_effects` of the current settings.
- `pass`: the pass's row in the runtime renderer's frame graph: `registered`, `runs` (frames it was
  called in, idle or not), `failures`, `last_frame`, `last_error`, `last_cpu_ms`; `null` in an app
  without a runtime renderer.
- `counters` (process-wide): `views_recorded` (views the pass drew effects for), `views_idle` (views
  it skipped because nothing was asked), `aberration_skipped` (views whose colour target could not
  be copied, so the aberration was left out), `last_ghosts` (ghosts in the last recorded view), `targets_built`
  (sets of bright and ghost images built, one per view size in use: it must stand still on a steady frame) and
  `tables_traced` (ghost tables of the built-in lens traced so far).
- `lens`: the traced table at the effective coating: `name`, `coating`, `ranked` (ghosts above the
  trace's cutoff), `ghosts` (at most 8, most visible first, energy per footprint area: `first_bounce`, `second_bounce`, `energy`
  `[r, g, b]`, `magnification` `[r, g, b]`, `radius` as a fraction of the frame width).
- `light` (echoed) and `predicted` (`null` without `light`): per ghost, `first_bounce`, `second_bounce`, `at`
  (the centred frame position `m · light` at the green wavelength), `in_frame` and `radius`. Compare a screenshot
  against it: a bright source at `light` puts each ghost's centre at `at`.

### `render.set_lens_effects`

| Field | Value |
|---|---|
| `entity` | Camera entity id (default the active window camera). |
| `flare_intensity` | 0–1000. 1 draws the ghosts with the energy the trace gives them; small sources need more. 0 is off. |
| `flare_threshold` | 0–65 504: the exposed luminance (scene luminance × the view's exposure; 1 is display white) above which a pixel casts ghosts. Default 4. The same highlight casts the same flare as an auto exposure adapts. |
| `coating` | 0–1: the trace's coating dial (1 as coated, dim ghosts; 0 bare glass, bright ghosts). The built-in lens snaps it to steps of 1/16. |
| `dirt_intensity` | 0–10, 0 off. |
| `chromatic_aberration` | 0–0.05 of the centre-to-corner radius, 0 off. |
| `starburst` | 0–10, 0 off: diffraction spikes (six, one per iris blade) from bright pixels. |
| `after_dof` | `true` runs the pass after the camera's depth of field and motion blur (default `false`: before them). |
| `remove` | `true` removes the component (no other strength may be given with it). |

Fields left out keep their value (or the default for a new component). Result:
`{ entity, created, removed, before, after }`.

## Errors

Codes are `F0002` (bad params), `F0003` (missing camera) and `F0001` (internal); `data.reason`
says which:

| Reason | Code | When |
|---|---|---|
| `camera_missing` | F0003 | No window camera and no `entity` given. |
| `not_a_camera` | F0003 | The entity has no `Camera`. |
| `unknown_field` | F0002 | A param the method does not take. |
| `invalid_value` | F0002 | A param of the wrong type or out of range (`light` not two numbers in −4..4), or `remove: true` with strengths. |
| `nothing_to_remove` | F0003 | `remove: true` on a camera without `LensEffects`. |
| `trace_failed` | F0001 | The built-in lens failed its validation (a bug; report it). |

## Example: check that a sun flare draws

```json
{"method":"render.set_profile","params":{"preset":"desktop_high"}}
{"method":"render.set_lens_effects","params":{"flare_intensity":4.0,"dirt_intensity":0.5,"chromatic_aberration":0.003}}
{"method":"play.step","params":{"frames":2}}
{"method":"render.lens_effects","params":{}}
```

`effective.active` is `true`, `pass.runs` grows each frame and `counters.views_recorded` grows
on frames that drew. A screenshot (`seeing.screenshot`) with the sun in frame shows the ghosts on
the line from the sun through the centre. With `preset_allows: false` the effects stay off: change
the preset first.

## Limits

- The flare is screen-space: a light outside the frame casts no ghost, and a light leaving the
  frame takes its ghosts with it over the outer 5 % of the frame.
- Ghosts and the starburst are images of one built-in iris (six blades); the lens is the built-in
  synthetic one at f/22 (no lens picker yet). The bright image is reduced to a 16 × 9 grid of cells,
  so two lights in one cell share their ghosts, drawn at their centroid.
- The game-frame warm builds the pass's pipelines and traces the ghost table at the default coating (not on
  Android, where no preset allows the pass); its targets and the dirt mask are created on the first frame that
  asks for an effect (counted by the hitch gate), and another coating step is traced on the first frame that
  uses it. The targets go again a few frames after the last view of that size asked.
- Ghost and dirt light is capped at 4096 × display white per texel, so the sum with the scene stays finite.
- The pass runs before the camera's depth of field and motion blur by default, so they also soften the ghosts;
  `after_dof: true` (ADR-3196) moves it behind them, together with the frame's other `before_post` host
  passes, when the camera's lens runs that frame (else it stays where it was). The starburst gathers six
  rays of 10 taps from a coarse mip of the bright image, about 60 reads per output pixel.

## Saved in scenes

`LensEffects` is a reflected component (`regia_render::lens_effects::LensEffects`, format `lens-effects` v1,
`docs/formats/lens-effects-v1.md`): a camera's effects set with `render.set_lens_effects` are saved with the level
and come back on load. `regia upgrade-project` lists and rewrites it like the other scene components; a component
from a newer engine is ignored by the renderer with one warning.
- A frame that draws an effect copies the HDR image once (the pass draws on that copy, so it never feeds
  back into the temporal history or the held-screen cache).
