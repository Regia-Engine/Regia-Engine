# Mesh debris and impact sites

Throw glTF debris from an effect on arcs that are a pure function of the frame number, lay a
crater with burnt ground, and swap a hull to its battered variant at the impact frame. These need
`regia_vfx::DebrisPlugin` (bursts, the clock and the mesh library) and, for sites,
`regia_vfx::ImpactSitePlugin` and `VfxPlugin` (the decal pool and the puff). Without them every
call returns `UNSUPPORTED` (`F0016`).

The game registers its fragment meshes in `DebrisMeshLibrary`, one or more variants per class
(`earth`, `stone`, `turf`, `hardware`), each authored about 1 m across; the fragment size scales
them. A class with no mesh spawns no fragments, and `vfx.debris_state` shows the counts per class
under `meshes`.

## The clock

Debris and sites are posed for the frame on `DebrisClock`. By default it advances one frame per
update, and a paused app holds it (ADR-2633). A host that owns a frame number (a sequence player, a render-out loop) sets `auto: false` and
writes the frame itself. `vfx.debris_clock` does the same over the wire:

```json
{ "method": "vfx.debris_clock", "params": { "frame": 240, "auto": false } }
→ { "before": { "frame": 31, "auto": true }, "after": { "frame": 240, "auto": false } }
```

A fragment's entity exists exactly when its pose does, so a seek in either direction shows the
scene that playing to that frame would show: a fragment past its lifetime is gone, and a seek back
brings it back.

## Throw a burst

`vfx.debris_burst` creates a burst. `origin` is required; `seed` defaults to 1, `start_frame` to
the clock's frame, `tier` to `High`, `ground_y` to the origin's height (the fragments land on that
flat plane), `gravity` to 9.81 and `fps` to 60. `counts` overrides the per-class counts before
the tier cap (default 520 earth, 380 stone, 300 turf, 36 hardware: 1236 pieces).

```json
{ "method": "vfx.debris_burst", "params": { "origin": [0, 0, 0], "seed": 7, "start_frame": 120, "tier": "High" } }
→ { "burst": 0, "fragments": 1236, "classes": { "earth": 520, "stone": 380, "turf": 300, "hardware": 36 },
    "start_frame": 120, "tier": "High", "cheap": false, "meshes": { "earth": 4, "stone": 4, "turf": 3, "hardware": 4 } }
```

`remove: <id>` deletes a burst and its entities. At most 8 bursts live at once.

The physics, all closed form: a fragment flies the parabola `p + v t - g t² / 2` with optional
linear drag, lands on the first ground crossing on its way down, keeps 0.35 of its horizontal
speed at touch-down and stops in `(0.35 v)² / (2 μ g)` meters (μ floored at 0.05, at most 6 m),
eases its tumble to a resting tilt and rests partly sunk by its class's burial
fraction. Nothing bounces and nothing trails flame. Only hardware flagged molten at launch glows,
with `e^(-t / 0.7 s)`, dark after 3 s.

**Tiers.** The tier caps the fragment count: MobileLow 48, MobileHigh 128, Deck 192, Low 256,
Medium 768, High 1536, Ultra 4096. When the cap bites, hardware keeps at most `max / 4` plates
and the other classes share the rest in proportion, which can raise a class above the count the
host set. An all-hardware burst stops at the quarter. A burst already within the cap is unchanged.
Integer division can leave the total a few under the cap. A lower count keeps the arcs
of the pieces that remain. Low, Deck and Mobile also use cheap detail: no molten glow and no
shrink fade (`"cheap": true`).

## Check determinism

`vfx.debris_state` returns every burst with a `hash` of all its poses at `frame` (default the
clock's). The hash is a pure function of the burst and the frame, so two runs with the same seed
agree, and `frame` lets one run compare any frame without stepping to it:

```json
{ "method": "vfx.debris_state", "params": { "frame": 300 } }
→ { "clock": { "frame": 31, "auto": true }, "frame": 300, "meshes": { "earth": 4 },
    "bursts": [ { "burst": 0, "start_frame": 120, "fragments": 1236, "live": 0, "landed": 910, "hash": "5d1c0e7a39b2f604" } ],
    "sites": [] }
```

The values above are illustrative. `live` counts fragment entities alive at the clock's frame; `landed` counts fragments on the
ground at `frame`.

## Impact site

`vfx.impact_site` schedules a symmetric crater at `center` (the strike point on the pre-impact
ground). Defaults: `at_frame` the clock's frame, `radius` 27 m (a 54 m crater), `depth` 14 m,
`rim_height` 4 m, `blanket_radius` 150 m, `scorch_radius` 300 m, `puff_lead` 4 frames. `detail`
is `minimal`, `reduced` or `full`; omitted, it comes from `tier` (Low, Deck, Mobile: minimal;
Medium: reduced; High, Ultra: full).

```json
{ "method": "vfx.impact_site", "params": { "center": [120, 10, -40], "at_frame": 600 } }
→ { "site": 0, "at_frame": 600, "puff_frame": 596, "detail": "full", "decals": 5, "floor_y": -4.0,
    "rim": { "height": 4.0, "radius": 27.0 }, "blanket_radius": 150.0, "scorch_radius": 300.0 }
```

At `puff_frame` the effect named `impact_first_contact_dust` plays at the strike point if the
effect library has one (the small dust puff at first ground contact; a game or another lane
registers the `.vfx.ron`). At `at_frame` the decals go down: wide soot, a nearer burn, the ejecta
blanket, the crater floor and a black core with an ember that cools in a few seconds. There is no
gouge decal: a steep fall at terminal speed digs a round pit. A seek back past `at_frame` retires
the decals.

The decals only colour the ground. For the pit itself, the game adds `ImpactSiteSpec::height_delta(r)`
(floor `-depth`, crest `+rim_height`, a blanket falling as `r⁻³`) to its terrain heights with
`TerrainMap::set_height`; the result reports `floor_y` and the rim.

**The hull swap.** An entity with a `DamageSwap { at_frame, damaged_mesh, damaged_material,
damaged_transform }` component wears the battered mesh and pose from `at_frame` on, and the intact ones
before it or after a seek back. `ImpactSiteSpec::buried_transform(half_height, sink, tilt, yaw)`
gives the rest pose: `sink = 0.5` puts the hull's middle at the crater floor, half buried, leaning a
little.

`remove: <id>` deletes a site. A game can also schedule a site from an effect: a `SpawnEffect`
named in `ImpactSites::trigger_effects` (default `impact_site`) schedules `ImpactSites::template`
at the effect's position on the current frame.

## Errors

- `F0016` (`UNSUPPORTED`): the plugin is not added.
- `F0002` (`INVALID_PARAMS`): a missing or malformed field, a number out of range (`origin` and
  `center` within 1e6 m, `gravity` 0.1..=100, `fps` 1..=1000, `counts` at most 8192 each), an unknown
  `tier`, `detail` or class name, an unknown id to `remove`, a ninth burst or site, or a site
  whose dimensions are not ordered (`radius < blanket_radius <= scorch_radius`).

## Cost

Per frame, a burst costs one closed-form pose and one transform write per live fragment, plus the
renderer's draw of the instances (fragments of one variant share a mesh and material and batch). On an RTX 5090 a
1236-piece burst in flight added about 2 ms per frame (render plus readback) at 1440p and 4K and about 0.3 ms of CPU.
