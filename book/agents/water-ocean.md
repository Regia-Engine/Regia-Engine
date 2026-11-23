# FFT ocean

A wind sea on a plane water body: a directional spectrum, evolved by a compute pass and turned into
tiling maps by an inverse FFT. Put an `FftOcean` on `WaterBody::ocean`, or from the protocol:

```json
{ "method": "water.set", "params": { "ocean": { "wind_speed": 14.0, "seed": 7 } } }
```

Any field you leave out keeps its value (or the default). `{ "ocean": false }` removes the ocean.
The body must be a plane. See [Water bodies](water.md) for everything else about water.

The ocean is off until you ask: a new body has none, draws no compute and looks as it always did.

## Swell or ocean

`water.set_mode` switches one plane between its trochoidal swell and its ocean, and keeps both:

```json
{ "method": "water.set_mode", "params": { "mode": "fft", "grid": 512, "ocean": { "wind_speed": 18.0 } } }
{ "method": "water.set_mode", "params": { "mode": "swell" } }
```

| Param | Meaning |
|---|---|
| `mode` | `fft` or `swell`. Required. |
| `entity` | The body. Default: the first. |
| `grid` | `fft` only. Texels per side of the drawn maps: 128, 256, 512 or 1024, or 0 to let the tier choose. |
| `ocean` | `fft` only. A field patch over the ocean. |

`swell` parks the ocean: queries and drawing go back to the body's swell, and the tuned ocean waits for the next
`fft`. A body with no swell gets the ocean's stand-in while it is parked, and a round trip restores the body exactly. A
calm plane stays calm in `swell` mode; give it waves with `water.set { swell }`.

<!-- not-a-request -->
```json
{ "entity": 4294967310, "before": "swell", "mode": "fft", "tier": "high", "ocean_draw": true, "drawn_size": 512,
  "ocean": { "seed": 1, "grid": 512 }, "swell": null, "parked_ocean": false }
```

`drawn_size` is what draws right now: 0 on Low or with `ocean_draw` off, whatever the grid says. `water.state` reports each
body's `mode` (`swell`, `fft`, or null for a ribbon) and `parked_ocean`.

## What you get per tier

| Tier | Maps | What it draws |
|---|---|---|
| `low` (Steam Deck, mobile, Desktop Low) | none | The swell: the body's, or a stand-in fitted to the spectrum. No compute. |
| `high` | 3 bands of 128² | The surface the queries answer. |
| `ultra` | 3 bands of 256² | The same waves plus finer ones in the shortest band. |

`water.set { ocean_draw: false }` draws the swell on any tier, for a side-by-side look. It does not
change what gameplay feels: queries keep answering the ocean. To move gameplay as well, use `water.set_mode`.

`grid` replaces the map size on `high` and `ultra`; `low` stays without compute. Queries use the 128 grid at any
setting, so a larger grid costs more and looks finer, and a boat rides the same sea. The bands keep their waves: a larger
grid adds finer waves to the shortest band, down to about 13 cm at 1024. A device that cannot hold a grid draws the swell and
logs one warning; `last_frame.ocean_dropped` counts it.

One spectrum runs per frame. Bodies with an equal `FftOcean` share it. A body with a different one
draws its swell, and `water.state` counts it in `last_frame.ocean_dropped`. The count also holds every
ocean body of a frame that draws no ocean at all: on `low`, with `ocean_draw: false`, or on a device without the
compute path.

## Fields

| Field | Range | Default | Meaning |
|---|---|---|---|
| `seed` | whole number | 1 | Same seed and same water clock give the same sea, on any machine. |
| `spectrum` | `jonswap`, `phillips` | `jonswap` | JONSWAP is a fetch-limited sea with a sharp peak; Phillips a saturated sea set by the wind alone. |
| `wind_speed` | 0.5..=40 m/s | 12 | Wind 10 m above the water. |
| `wind_direction` | `[x, z]`, not zero | `[1, 0]` | Direction the wind blows toward. Waves run this way. |
| `fetch` | 1 000..=2 000 000 m | 300 000 | Open water the wind has crossed (JONSWAP). Longer is higher and longer. |
| `gamma` | 1..=7 | 3.3 | JONSWAP peak sharpening. 1 is a fully developed sea. |
| `spread` | 1..=64 | 8 | Larger is more long-crested. |
| `choppiness` | 0..=2 | 0.9 | Horizontal offset. 0 is a height field; sharper crests above that. |
| `height_scale` | 0..=4 | 1 | Scale on the heights. |
| `tiles` | 8..=20 000 m | `[1009, 257, 67]` | The three bands' tiles, longest first, each at least twice the next. |
| `crest_foam` | 0..=4 | 1 | Foam strength. |
| `foam_onset` | 0..=1 | 0.15 | Compression `1 − J` where foam starts. Lower foams more. |
| `shore_start` | `shore_end`..=1000 m | 24 | Depth from which the whole ocean is there. The default `shore_end` is 3, so `shore_start: 1` on an otherwise default ocean is `F0002`. |
| `shore_end` | 0..=1000 m | 3 | Depth at which only the swell is left. `shore_start` must be at least this. |
| `grid` | 0, 128, 256, 512, 1024 | 0 | Texels per side of the drawn maps. 0 lets the tier choose. |
| `foam_decay` | 0..=120 s | 0 | Lingering foam: seconds full foam takes to fade. 0 keeps the foam instantaneous and costs nothing. See [foam](#foam). |
| `foam_growth` | 0..=30 s | 0.5 | Seconds a breaking crest takes to leave about two thirds of its foam behind. Only read with `foam_decay` on. |
| `whitecap_rescale` | `true`, `false` | `false` | Keep the whitecaps' share of the surface in the distance. See [foam](#foam). |

Tiles that are not whole multiples of each other put off the repeat: the default bands, all prime,
only line up every 17 000 km. Each band alone repeats on its own tile.

## Queries are the same on every tier

`water.surface`, `water.raycast`, `WaterBody::surface`, `WaterQuery` and the floating bodies answer from
a CPU copy of the compute pass: the same waves through the same inverse FFT, at 128². High draws exactly
that surface. Ultra adds waves shorter than about 1 m. The answer does not depend on the tier, the frame
rate or what is on screen: it is a function of the seed and the water clock's tick.

**On Low a floater rides a surface that Low does not draw.** The stand-in swell has the same height and
peak wavelength, but not the same crests. Keep this in mind for boats on Steam Deck and mobile.

A single-player game can turn on `water.set { queries_follow_tier: true }` (`WaterSettings::queries_follow_tier`).
Queries then answer the surface the tier draws: on Low, or with `ocean_draw` off, an ocean answers its stand-in
swell, and a swell only the waves the tier draws. A floater on Low then sits on the drawn vertices within 5 mm near
the camera. It is off by default because a networked game needs every machine to agree where a boat is, whatever
its graphics settings. The queries follow what the renderer actually drew, not only the tier (ADR-3251): an ocean
the device cannot run (a 1024 grid with a lingering-foam history needs a 151 MB storage binding; a 128 MiB device
refuses it, `last_frame.ocean_refused` names the limit) or a second ocean with another spectrum draws as swell, and
the queries answer that swell (`WaterDrawnOceans`). The water plugin keeps each body's drawn surface in a cache
(`DrawnWaterCache`), so a steady query costs no more allocations than with the switch off.

`water.ocean_cost` names a grid the device cannot run: the row has `ran: false`, `refused: { code, grid, needed?,
limit, history? }` (`code` one of `storage_binding`, `buffer_size`, `texture_dimension`, `workgroups`) and `error`, the
message. Check it on a small device: `water.ocean_cost { grids: [1024] }` with a body whose
`ocean.foam_decay` is above 0 on the mobile min-spec profile reports `storage_binding`.

The first query of a tick runs the CPU transform; the rest of that tick share it, and a tick with no
query costs nothing.

Every `ω` is a whole multiple of `2π / 2048 s`, so the sea repeats exactly every 2048 s and its phases are
exact integers, however long the session runs.

## Near the shore

With a [shore field](water.md#shore), the surface blends by the depth under each point: the ocean from
`shore_start` out, the shoaled swell inside `shore_end`, a crossfade between. A body with no swell of its
own gets a stand-in, so a beach still gets waves. Without a field the ocean runs everywhere: a tiling map
cannot shoal, so give a coast a field.

## Foam

Foam comes from the Jacobian of the horizontal offset: where the surface compresses toward a crest. By default
it is a pure function of the tick and appears and vanishes with the crest.

**Lingering foam.** Set `foam_decay` (seconds) and foam stays where a crest broke, then fades. Each band keeps a
history map on its own tile, stepped once per frame after the transform: it grows toward the crest's foam
(`foam_growth`), and once the crest has gone it falls by a straight line to zero in `foam_decay` seconds from full.
The map is anchored to the water, so the foam rides the swell's orbits. The frame draws the larger of the crest's
foam and the lingering foam. `water.surface` keeps answering the instantaneous foam, so queries never depend on
the frames drawn before.

The history starts over from no foam on a camera cut, when the water bodies change, when the water clock steps
back or jumps more than a second, when the ocean or the grid changes, and on `WaterRenderer::reset_ocean_foam`.
`WaterPlugin` hands the frame renderer's cut count to the water every frame, so a cut detected while a frame is
drawn restarts the history on the next one. It costs one dispatch per band and `2 × 3 × N² × 16` bytes: 1.6 MB on
High, 6.3 MB on Ultra, 25 MB at 512 and 100 MB at 1024. An ocean without it allocates nothing. At 1024 one work
buffer then needs a 151 MB storage binding; a device that cannot bind that much draws the ocean as swell and counts it
in `last_frame.ocean_dropped`.

**Whitecaps in the distance.** Far away, each pixel covers several of the short waves, so the shader fades those
bands, and the compression they made goes with them: whitecaps thin out toward the horizon. With
`whitecap_rescale: true` the shader scales the compression of the bands it keeps by how much spread the faded
bands took away (at most four times), so the share of the surface in whitecaps stays about the same with distance.
It restores how much foam there is, not the shape of the small whitecaps.

A larger `grid` foams more at the same `foam_onset`, because finer waves compress the surface more. At 14 m/s the share of the
surface past the onset is 10.5 % at 128, 13.2 % at 256, 15.3 % at 512 and 16.9 % at 1024, before the distance fade. To keep the same
whitecaps at a larger grid, raise `foam_onset`.

The three bands each hold displacement, slopes (the normal map) and the fields of the Jacobian (the foam map). In Rust,
`OceanMaps::displacement`, `band_sample` and `band_foam` read them per texel from `WaterRenderer::read_ocean_maps`.

## Methods

`water.ocean { entity? }` reports each ocean body, in entity order.

```json
{ "count": 1, "tick": 600, "seconds": 10.0, "tier": "high", "ocean_draw": true, "oceans": [{
    "entity": 4294967310, "ocean": { "seed": 7 }, "wave_height_m": 3.4, "expected_height_m": 3.6,
    "peak_wavelength_m": 138.0, "peak_period_s": 9.4, "loop_s": 2048.0, "query_size": 128,
    "bands": [{ "tile_m": 1009.0, "k_min": 0.0, "k_max": 0.2, "modes": 2600, "variance_m2": 0.6,
                "mean_square_slope": 0.004, "rms_wavelength_m": 120.0 }],
    "max_rise_m": 3.2, "max_shift_m": 3.8, "folded_share": 0.0, "drawn_size": 128 }] }
```

`wave_height_m` is this seed's significant height; `expected_height_m` is the spectrum's. They differ by
a few percent because a tile holds few of the longest waves. `folded_share` is the share of the surface
whose crests fold over (`J < 0`): keep it near 0 with `choppiness`. `drawn_size` is the map size the
tier draws, 0 for the swell alone.

`water.ocean_check` reads the maps back from the GPU in use and compares every field of every band with
the CPU copy at this tick. It blocks until the GPU is done, so use it for checks, not per frame.

```json
{ "drawn": true, "tick": 600, "size": 128, "worst_error": 0.00002, "worst_field": "h", "worst_band": 0,
  "peak_height_m": 1.9 }
```

Expect a `worst_error` well under 0.001. `{ "drawn": false, "tick": 600 }` means no ocean is drawn on this
tier. It compares at the grid in use, so at 1024 it holds several hundred MB of CPU memory while it runs.

`water.ocean_cost { grids?, frames?, entity? }` times the compute pass on the GPU in use. It runs on a simulation of its
own, so the frame's maps do not move. The grids default to 256, 512 and 1024, the frames to 32 (at most 256). The
ocean is `entity`'s, else the first body's, else the default. It blocks until the GPU is done, for every frame, so use it
for checks, not per frame.

<!-- not-a-request -->
```json
{ "ocean": { "seed": 1 }, "frames": 32, "costs": [
  { "size": 512, "ran": true, "frames": 32, "dispatches": 57, "modes": 267096, "gpu_bytes": 100700000,
    "compute_ms": 0.25, "copy_ms": 0.19, "submit_ms": 0.7, "tables_ms": 126.0, "upload_ms": 15.0 } ] }
```

`compute_ms` and `copy_ms` are medians by GPU timestamps, null on a device without encoder timestamps. `submit_ms` is the wall
time of the whole evolve and an upper bound on its GPU time. `tables_ms` and `upload_ms` are paid once per ocean and grid, on
the first frame that needs them. `ran: false` means this device cannot run that grid. See [what it costs](#what-it-costs) for
measured numbers.

With lingering foam on (`foam_decay` > 0), each row also counts the history: `history_bytes` (included in
`gpu_bytes`) and `history_dispatches` (included in `dispatches`, three). Both are 0 with it off.

`water.state` reports `bodies[].ocean` (with `foam_decay_s`, `foam_growth_s` and `whitecap_rescale`),
`ocean_draw`, `queries_follow_tier` and, under `last_frame`, `ocean_bodies`, `ocean_dropped`,
`ocean_size`, `ocean_dispatches`, `ocean_evolves`, `ocean_history_bytes`, `ocean_history_steps` and
`ocean_history_restarts`. `render.graph` lists the compute
pass as `water_ocean_fft` at `after_opaque`, with its run count.

## Errors

| Code | When |
|---|---|
| `F0002` | A bad field: out of range, unknown, tiles not falling by a factor of two, a grid that is not 0, 128, 256, 512 or 1024, an ocean on a ribbon, an unknown mode, `grid` or `ocean` with mode `swell`, bad `grids` or `frames`. Nothing changes. |
| `F0003` | `water.ocean { entity }` or `water.ocean_cost { entity }` on a body with no ocean; `water.ocean_check` or `water.ocean_cost` with no water renderer; `water.set` or `water.set_mode` without `WaterPlugin` or without the body. |
| `F0001` | Internal: the map readback of `water.ocean_check` or a `water.ocean_cost` measurement failed on the device, or a reply could not be serialized. Nothing changes. |

## What it costs

| Grid | Dispatches | Modes | GPU memory | Compute | Copy | Frame against the swell | Set-up, once |
|---|---|---|---|---|---|---|---|
| 128 (High) | 45 | 22 104 | 6 MB | 0.09 ms | 0.02 ms | +0.10 ms | 18 ms |
| 256 (Ultra) | 51 | 71 000 | 25 MB | 0.11 ms | 0.06 ms | +0.17 ms | 47 ms |
| 512 | 57 | 267 096 | 101 MB | 0.25 ms | 0.19 ms | +0.50 ms | 141 ms |
| 1024 | 63 | 1 052 504 | 403 MB | 1.10 ms | 0.75 ms | +1.84 ms | 607 ms |

RTX 5090 (one card, driver 615.71.09, Vulkan, P0), 1920 × 1080, 2026-11-02. Compute and copy are medians by GPU timestamps
over five turns of 64 evolves; a turn repeats to within 0.5 %. The frame column is the frame's GPU time against the same
body drawing its swell, medians of 80 frames; two identical 128 setups differ by 0.02 ms, which is the resolution here. The scene
is small (0.4 ms on its own), so read the column as what the ocean adds, not as a share of a real frame. Set-up is CPU time for the
tables and the upload, on a host at load 75 at nice 15, so an upper bound. At 1024 it is a visible hitch on the first frame.

These numbers are for one card. Run `water.ocean_cost` on yours. The example
`crates/regia_terrain/examples/water_ocean_cost.rs` prints the same table, with `REGIA_GPU_UUID` to pick a card:

```text
cargo run --release -p regia_terrain --example water_ocean_cost -- --grids 128,256,512,1024 --size 1920x1080
```

## A host of your own

`WaterPlugin` does all of this. If you drive `FrameRenderer` yourself, install `WaterRenderer::layer()` as
for any water and register `WaterRenderer::ocean_pass()` with `FrameRenderer::register_pass`. Without the
pass the layer still evolves the maps; only the `render.graph` row is missing.
