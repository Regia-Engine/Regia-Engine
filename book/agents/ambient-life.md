# Ambient life

Ambient life is the small motion that keeps an outdoor scene from feeling empty (ADR-3024). It covers:

- motes (pollen and dust) glinting in the sun,
- gnat and fly swarms,
- butterflies,
- fireflies at dusk,
- falling leaves by season,
- birds: flocks and single soaring birds.

The bird calls of the audio's ambience can follow the visible birds, so a call is heard where a bird is seen.

Ambient life is **off by default**. Nothing runs and nothing is drawn until a project or an agent turns it on.
It is also off on the Low tier and below (Low, Deck, mobile). A host without it draws the very same frames:
`crates/regia_vfx/tests/ambient_frame.rs` checks this bit for bit.

## Turning it on

```json
{ "method": "render.set_ambient_life", "params": { "enabled": true } }
```

`render.set_ambient_life` takes these parameters, all optional:

| Parameter | Default | What it does |
|---|---|---|
| `enabled` | `false` | Run and draw ambient life. |
| `motes`, `insects`, `birds`, `leaves` | `true` | Each kind on its own. `insects` covers swarms, butterflies and fireflies. |
| `density_scale` | `1` | Multiplies every drawn count, `0..4`. |
| `season` | `auto` | `auto` follows the time of day's date and hemisphere. Or `spring`, `summer`, `autumn` (or `fall`), `winter`. |
| `seed` | `0` | Seed of every placement and flight, the automatic birds' too (a new seed restarts their flight). The same seed gives the same frames. |
| `auto_birds` | `true` | Without any placed flock, put a flock and a soaring bird ahead of the view. Turning it off removes them. |

It answers `{ before, after }`, both in the shape of `render.ambient_life`. A host sets the same switches
through the `regia_vfx::AmbientLifeSettings` resource.

## What sets the density

The scene's time, season and weather set an activity of `0..1` for each kind. The tier's budget times the
activity times `density_scale` gives the drawn count. Counts are real numbers, so creatures fade in and out
as the weather changes and never pop.

| Kind | Active when | Gone when |
|---|---|---|
| Motes | daylight, sun through the cloud; more pollen in spring and summer | night, from about 0.6 mm/h of rain or snow, from 9 m/s of wind |
| Swarms | warm season (most in summer), day and evening | winter, rain, from 4 m/s of wind |
| Butterflies | spring and summer, the sun above about 6°, not overcast | night, rain, from 6 m/s of wind |
| Fireflies | summer dusk, from just before sunset to about 17° below the horizon | day, deep night, winter, rain |
| Leaves | autumn (a few in other seasons); wind strips more | never fully |
| Birds | day (a few at night) | from 1 mm/h of rain or 12 m/s of wind |

The rain and wind limits for birds are the same ones at which the audio's bird calls fall quiet.

| Tier | Motes | Swarm insects | Butterflies | Fireflies | Leaves | Birds |
|---|---|---|---|---|---|---|
| Low, Deck, mobile | 0 | 0 | 0 | 0 | 0 | 0 |
| Medium | 1,500 | 160 | 8 | 128 | 300 | 16 |
| High | 3,000 | 320 | 16 | 256 | 600 | 32 |
| Ultra | 6,000 | 640 | 24 | 512 | 1,200 | 64 |

## How it is drawn

Motes, insects and leaves have no simulation buffer. Each creature is a small instanced quad (a butterfly
has two wing quads) in a box wrapped around the camera, the same scheme the rain uses. The vertex shader
rebuilds each creature from its index and the seed. The CPU integrates one drift per box on game time and
uploads one uniform per frame, and steady frames allocate nothing (`tests/ambient_alloc.rs`).

- A creature keeps its world position as the camera walks.
- A camera cut shows a full field on its first frame.
- The animation clock loops every 256 s. Every motion has a whole number of cycles per loop, so the loop
  never shows.

Each kind looks like this:

- **Motes** are soft flecks. Their 2.5 to 8 mm is what a lens sees, the flake with its glare, not its body: true to a
  millimetre flake's size they changed about 200 pixels of a 1440p forest frame, and no one could see them. They
  scatter the sun broadly to the side and strongly forward, so they brighten as you look toward it, and the light per
  pixel is capped at a white surface in the sun. They ask the renderer for its occlusion view (the one rain uses)
  aimed along the sun the frame draws, and only motes the sun reaches glint: shafts between the trees, nothing in the
  shade. That holds down to a golden hour: the view tilts as far as a 10° sun, and a lower sun keeps that tilt. Falling
  rain and snow and a wet or snowy surface keep the view, so on those frames motes glint without shade. Use
  `density_scale` for more or fewer.
- **Gnats** dance in balls about a metre across, within about 11 m of the eye. In the sun their wings glint like motes and shimmer as they beat; in the shade they are dark specks. Wind thins them from 3 m/s and they are gone at 6 m/s.
- **Flies** loop wider in groups of four.
- **Butterflies** meander around a home point within about 12 m of the eye, 6 to 10 cm across. Each wing is drawn with a pointed
  forewing and a scalloped hindwing, a dark margin and tip, veins, pale spots and a body, in temperate colors. They flap at 5 to 9 Hz and glide about a third of the time.
- **Fireflies** flash for about half a second every 2.5 to 6 s, as a soft cold yellow-green glow. At any moment about one in
  nine is lit.
- **Leaves** fall at 0.9 to 1.7 m/s. They sway and rock like a falling plate, drift with the wind and tumble,
  in their season's colors.

A render-out job restarts the animation clock, the drifts and the birds' flights, so two fresh apps deliver the
same frames however long each ran before the job and wherever the camera was. The automatic birds take the job's camera
from the first frame it draws, so they are not in the very first frame of a job with no warm-up. Pausing the weather
holds the sky, not the creatures: they stop only with the game clock.

The automatic flock starts about 80 m ahead of where the camera looks, 32 m up, near the top of a level view, over a forest's crowns, with a crow's
0.7 m wingspan. The soaring bird starts about 100 m ahead. They do not follow the camera's turns, only its travel: past
300 m they come back ahead of it.

Birds fly on the CPU at a fixed 60 Hz step, seeded. A flock steers by separation, alignment and cohesion
toward a target that runs along a closed spline around its anchor. A soaring bird circles on a thermal and
mostly glides. Each bird is two wing quads and a body facing the eye, so it reads from every side; a wing seen on its
edge keeps a width. Far birds keep a wing of at least 2 pixels and fade instead of vanishing.

## Reading it

`render.ambient_life` answers:

- whether ambient life is `enabled` and `active` (drawing), the `quality_tier`, and its `budget`,
- the `season` drawn and the `season_setting`,
- the kinds that are on, `density_scale`, `seed` and `auto_birds`,
- the `conditions`: `sun_elevation_sin`, rain and snow in mm/h, `wind_m_per_s` and `cloud_coverage`,
- each kind's `activity`, the `drawn` instance counts and `birds_drawn`,
- `sun_view`: whether the motes or swarms asked for the occlusion view along the sun this frame, and
  `sun_view_tilt_deg`: its tilt from straight down when the last frame drew it, `null` when it did not (off,
  night, a tier without the view, or rain, snow or a wet surface holding it). `render.precipitation` and
  `render.surface_exposure` do not report this view: it says nothing about the sky over a surface.
- each flock: its `entity`, `mode`, whether it is `auto`, `anchor`, `birds`, `visible`, size and speed, its
  `call_sounds` and `call_reach_m`, its fixed `steps`, and the `positions` of its visible birds,
- `calls`: how many bird calls were `bound` to a visible bird, and how many were `unbound` (in reach, but
  no bird was visible).

## Birds and their calls

```json
{ "method": "render.set_ambient_birds",
  "params": { "position": [40.0, 0.0, -70.0], "preset": "flock", "call_sounds": ["bird_call_a", "bird_call_b"] } }
```

`render.set_ambient_birds` places birds around a point:

- `preset` is `flock` (12 small birds on a 30 m loop, 18 m up, 10 m/s) or `soaring` / `single` (one bird
  with a 1.2 m wingspan circling 45 m up).
- `count` (1 to 64), `radius_m`, `height_m`, `wingspan_m`, `speed_m_per_s` and `seed` override the preset.
- `{ "clear": true }` removes every placed flock.

Placing a flock replaces the automatic ones. It answers `{ entity, flock }`. A host adds a
`regia_vfx::AmbientBirds` component to an entity with a transform instead. Moving that entity carries the
flight along; changing the component restarts it.

To hear each call from a bird, give the flock the same point and the same sound names as the audio's bird
spot (`audio.set_ambient_spot { preset: birds }` in the ambience), and add
`regia_framework::ambient_bird_calls::AmbientBirdCallsPlugin`. Each new one-shot audio emitter that plays
one of the flock's `call_sounds` within `call_reach_m` of its anchor then moves onto the nearest visible
bird. It follows that bird while it plays, through `regia_vfx::FollowAmbientBird`, which any entity can
use. A loop (a stream) is never moved. A call with no visible bird stays where the audio put it.

## Errors

Errors are `F0002`, with `data.reason`:

- `ambient_unknown_season`: `season` is not `auto`, `spring`, `summer`, `autumn`, `fall` or `winter`.
- `ambient_unknown_preset`: `preset` is not `flock`, `soaring` or `single`.

Out-of-range numbers, a missing `position` and empty sound names are plain `F0002` errors.

## Cost

The draw is six instanced draws with no compute. Its textures are the scene depth and the occlusion atlas: about 26,000
vertices at High with everything on. The CPU side is one uniform write. The birds cost one O(n²) steering
step per flock per 60 Hz tick. At most 64 are drawn, but every placed flock flies whether or not it is drawn, so place
only what you need: about 20 µs a frame with 32 birds, 50 µs with 64. While the view along the sun is on, the renderer also tests each shadow caster against it, about 3 ns per
caster. Measure a card's GPU time on your own scene with `--gpu-timing`, with and without `--ambient-life`. On
lavapipe, the tests only prove what is drawn, not how fast.

One measurement: an RTX 5090 at 2560x1440 on the showcase forest, a summer afternoon, 2,885 creatures drawn. The
`post` pass, where the layer draws, moved by at most +0.009 ms with every kind on. Between two rounds with it off, the
same pass moved by 0.02 ms, so the cost was within the noise. Whether the view along the sun was drawn was not recorded
then; the bench now prints the share of frames that drew it, whose cost is in `shadows`.
The ignored test `bench_on_a_card` in `samples/showcase/tests/ambient_life.rs` repeats the measurement on any card. It
takes turns between the kinds inside one run.
