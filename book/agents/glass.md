# Glass: own canopy and damage

Per-instance glass control for transmissive meshes (`regia_render::glass`). Two components on a
glass mesh entity; entities without either draw exactly as before.

- `GlassView` — `exterior` (default) or `viewer_inside`: the pilot's own canopy or visor. That
  instance does not refract and its specular is scaled down to an anti-glare level
  (`inside_specular`, default 0.15). Crack shading stays on. Glass seen *through* it (another
  unit's canopy, a building window) still refracts: the refraction copy is taken before any glass
  draws, and the canopy only composites over it.
- `GlassDamage` — a crack set, a level D0–D3 and a list of impacts in mesh-local space. Each impact
  stamps one variant of the level's stamps into the instance's crack page at the hit point, so the
  cracks move with the vehicle or building. Saves keep the impact list; loading re-stamps it,
  byte-identical.

Crack sets are `.glasscrack.ron` v1 (`docs/formats/glass-crack.md`). Built-ins: `laminate` (unit
canopies: bullseyes and spall chips, then spiderweb with crazing and a milky halo, then heavy
crazing with small punched holes at the worst hits only; never sheds sheets or shards) and
`pressure_pane` (buildings: outer-pane star cracks, then a spiderwebbed outer pane over a clear
inner pane with parallax, then failure into chunky fragments and a decompression breach).

Refraction is on in Desktop High and Ultra (exterior glass), off on Low, Mobile and Deck.

## View mode

`render.glass_view { entity }` reads, `render.set_glass_view` writes:

```json
{ "entity": 4294967321, "mode": "viewer_inside", "inside_specular": 0.12 }
→ { "before": { "entity": 4294967321, "mode": "exterior", "inside_specular": 0.15 },
    "after":  { "entity": 4294967321, "mode": "viewer_inside", "inside_specular": 0.12 } }
```

`mode` must be `exterior` or `viewer_inside`; `inside_specular` is 0..=1.

## Own-canopy shading (v0.1.17)

Viewer-inside glass covers most of the screen, so its own light is shaded with a cheap shader by
default: the environment reflection with Fresnel, the sun (specular and diffuse, one cascade-0
shadow compare), the planet-shine fill, tint and absorption (the multiply pass is unchanged),
crack shading and the material's emission. It skips the clustered local lights, the fitted
emitters, probe GI and its reflection occlusion, the terrain and large-mesh occlusion terms, the
ground bounce, fog and multi-scatter. The visible difference: local lights and emissive panels
no longer put highlights on the own canopy. Exterior glass (windows, other ships' canopies) keeps
the full shader.

`render.canopy_glass` reads the process-wide setting, `render.set_canopy_glass` writes it from
the next frame (not persisted; one atomic change, so two callers setting different fields at once
both keep theirs):

```json
{ "shading": "full" }
→ { "before": { "shading": "cheap", "nearest_layer_only": false,
                "stored": { "shading": "cheap", "nearest_layer_only": false },
                "env_full": false, "env": "REGIA_CANOPY_FULL" },
    "after":  { "shading": "full", "nearest_layer_only": false,
                "stored": { "shading": "full", "nearest_layer_only": false },
                "env_full": false, "env": "REGIA_CANOPY_FULL" } }
```

- `shading`: `cheap` (default) or `full` (the forward shader, the look before v0.1.17).
- `nearest_layer_only` (cheap path only, default false): on frames with the glass front layer
  (screen-space refraction on), a canopy layer behind the nearest transmissive one adds no light
  of its own. Its tint still applies. A two-sheet canopy then loses the inner sheet's reflection.
- `REGIA_CANOPY_FULL=1` in the environment forces `full` over every setting (`env_full`
  true; `stored` shows what the host set).

The cheap path is for a camera inside the glass. A camera more than `eye_margin_m` (1 m) outside
the instance's bounding sphere (a chase, replay or texture-view camera while the instance keeps
`viewer_inside`) draws it with the full shader, so fog and every sun cascade apply. `render.canopy_glass`
counts the last frame's viewer-inside draws by the shader they took:

<!-- not-a-request -->
```json
{ "shading": "cheap", "nearest_layer_only": false, "eye_margin_m": 1.0,
  "last_frame": { "cheap": 0, "full": 0, "eye_outside": 1 }, … }
```

`last_frame` is `null` without a renderer. `cheap` 0 and `eye_outside` above 0 from the cockpit
means the glass's bounds do not hold the eye (a windscreen far from the pilot): the frame pays
for the full shader there.

Host code: `regia_render::set_canopy_glass(CanopyGlass { shading: CanopyShading::Full,
..CanopyGlass::default() })`, or `FrameRenderer::set_canopy_glass(Some(..))` for one renderer.
To compare, take a `seeing.screenshot` from the cockpit with each setting. The GPU cost of the
translucent section is `gpu_pass_ms.forward_glass` in `regia::frame_summary` (`telemetry.live`).

## Damage

`render.glass_damage { entity, set?, level?, stamp?, clear? }`:

- `stamp: { point, size?, energy? }` — `point` is the world hit (a ray hit on the mesh). It is
  moved into mesh-local space, mapped to the page (planar projection along the mesh's thinnest
  axis, or UV0 when the set says `damage_space: uv0`) and stamped now. `size` overrides the
  stamp's authored footprint in meters; `energy` feeds the set's escalation (default 1). With
  `level`, the stamp uses that level's stamps and raises the instance to it.
- `level` alone raises the level with one impact at the mesh center; `level: 0` clears.
- `set` switches the crack set (the impacts re-stamp with it). An unknown set is `NOT_FOUND`
  with the loaded names.

```json
{ "entity": 4294967321, "set": "pressure_pane", "level": 2,
  "stamp": { "point": [3.1, 1.4, -6.0], "size": 1.0 } }
→ { "entity": 4294967321, "set": "pressure_pane", "glass_type": "pressure_pane", "level": 2,
    "energy": 1.0, "impacts": 1, "applied": 1, "page_size": 512, "metres_per_uv": 2.0,
    "stamp": { "impact": 0, "local": [0.3, 0.005, -0.2], "uv": [0.65, 0.4], "texel": [332, 204] } }
```

Check the result with `seeing.screenshot`: cracks are frosted white lines (rough, scattering,
bending what is behind them), holes are cut out with jagged edges and cast no shadow.

## Breaches and fragments

When a `pressure_pane` reaches D3 (`GlassLevelChanged`), `regia_vfx::glass` bursts
`glass_chunks` (mesh fragments, 6–25 cm) and sends `GlassBreach { position, normal, atmosphere,
pressure }` keyed to the level's atmosphere. Each atmosphere plays its preset once
`(inside_kpa − outside)` is at least `min_pressure` (default 1 kPa): airless →
`glass_breach_airless` (vent plume of flash-frozen ice crystals and debris; with
`regia_framework::glass_breach::GlassBreachAudioPlugin` the breach sound emitter gets a
ground-coupled `AudioPropagation`, so it is felt, not heard), Mars → `glass_breach_mars`
(dust puff), Earth → `glass_breach_earth` (a faint gust). Earth outside is 101.3 kPa, so a pane
at 101.3 plays nothing. A pane at 200 kPa plays the gust, a shake clamped to 0.6 and a sound
emitter. A missing `GlassPressure` is treated as 101.3 kPa. `GlassPressure { outward, inside_kpa }`
on the pane sets the vent direction and the inside pressure. Games can send `GlassBreach`
themselves and override the presets in `GlassEffectSettings`; `GlassBreachFired` reports what
played and the sound emitter entity. `audio.propagation` on that emitter shows the paths.
