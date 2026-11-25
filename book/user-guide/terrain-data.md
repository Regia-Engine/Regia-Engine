# Terrain data: cooking and streaming

<!-- index: Terrain tile cache -->
<!-- index: Tiled heightfield -->
<!-- index: Terrain index -->
<!-- index: Virtual texture -->

The photoreal terrain **DATA path** turns a 4096² 16-bit heightfield and 8K macro / 2K–4K layer
textures into a streamable, byte-reproducible virtual-texture layout. It is the data half of the
terrain feature; the shading and CDLOD/clipmap half is `docs/specs/26-terrain-vfx-highend.md`.
Level cells and HLOD are a different stream: [World streaming](world-streaming.md)
(`docs/specs/78-world-streaming.md`). The task procedure is
[Author a terrain](../how-to/world/terrain.md).

## What cooks

`regia_asset_pipeline::terrain_data` (`cook_tiled_texture`, `cook_tiled_heightfield`) produces:

- **Tiled heightfield** — the R16 samples are kept 16-bit and split into 256-sample tiles, each
  with its own `[min, max]` for per-tile LOD bounds (`docs/formats/terrain.md` § 8).
- **Tiled textures** — 8K albedo (BC7 sRGB) / normal (BC5) / AO (BC4), 2 × RGBA 8K masks (BC7
  linear) and 6 layers × (albedo/normal/ORM/height) are split into 256-pixel tiles at every mip
  above the tile size, each tile a standalone KTX2 with a full mip chain, plus a resident tail for
  the coarsest mips (§ 7).

Every tile's `AssetId` is derived deterministically from the map id and its path
(`terrain/<slot>/mip<mip>/tile_<x>_<y>`), so the same input always cooks to the same bytes.

## Streaming

`regia_terrain::stream::TileCache` is an LRU tile cache with a byte budget and a background
loader thread. After `tick`, non-pinned tiles are evicted until the total fits the budget or
every remaining tile is pinned. Pinned tails are never evicted, so several tails that each fit
can together pass the budget. The render thread calls `request` (nearest tiles first) and `residency` each frame;
all file IO and decoding happens off-thread, so a frame never blocks on a load:

```rust
use regia_terrain::stream::{MemoryTileSource, TileCache, TileFormat, TileKey, TiledTexture};

fn main() {
    let camera = [0.0f32, 0.0f32]; // camera (x, z) in world meters
    let mut cache = TileCache::new(256 * 1024 * 1024, MemoryTileSource::new());
    cache.register_texture(0, TiledTexture::new("macro_albedo", 8192, 256, TileFormat::Bc7Srgb));
    // Each frame: enqueue the nearest tiles, then drain finished loads into the cache.
    cache.request(0, camera, 8192.0, 400.0, 64);
    cache.tick();
    let key = TileKey { texture: 0, mip: 0, x: 0, y: 0 };
    if let Some(bytes) = cache.residency(key) {
        // upload `bytes` (KTX2) to the GPU
        let _ = bytes;
    }
}
```

`PakTileSource` loads tiles (and each texture's resident tail) from the mounted paks / cooked cache
through the same deterministic ids the cooker wrote them under. The tail — the coarse mips below the
tile size — is pinned (`tail_key(slot)`). A tail that fits the budget stays resident and reachable
through `residency`, and it is not evicted. A tail larger than the whole budget is not cached,
`residency` stays empty for that key, and a later `request` does not try again.

`PakTileSource::new` applies the asset server's first-launch rule: a pak this build has not verified
yet (`pak_verify`) is read as copies, not views of its mapping, for the life of the source; the next
launch maps it. `PakTileSource::with_verify` takes the check options explicitly (`None` = check off).

## Cooking a terrain

A terrain is authored as a folder of source images plus one `terrain.layers.ron` file. `regia cook`
detects the `*.terrain.layers.ron` file and cooks the whole folder through the tiled path — the
generic single-texture path is **not** used for terrain inputs. The folder layout (a convention, not
a frozen-format change — see `docs/formats/terrain.md` § 9):

```text
assets/textures/
  terrain.layers.ron        layer stack (format v5; older v1..v4 files migrate through `upgrade-project`) (layer names + order)
  heightfield.png           16-bit gray R16 heightfield
  heightfield.json          optional world extents: `size_m`, `origin`
  macro_albedo.png          sRGB → BC7 sRGB
  macro/normal.png          tangent normal → BC5
  macro/ao.png              8-bit L → BC4
  macro/pack0.png           RGBA8 splat mask → BC7 linear
  macro/pack1.png           RGBA8 splat mask → BC7 linear
  detail/<name>/albedo.png  per-layer sRGB → BC7 sRGB
  detail/<name>/normal.png  per-layer linear → BC5
  detail/<name>/orm.png     R AO / G roughness / B metal → BC7 linear
  detail/<name>/height.png  linear L → BC4
```

Each layer in the `.ron` contributes four slots named `layer{index}_{kind}` (`layer0_albedo`,
`layer0_orm`, …). Missing optional slots warn and are skipped; a missing layer `orm`/`height` cooks
a neutral default (AO 1, roughness 0.8, metal 0; flat height). Every slot cooks into 256-pixel
tiles; the whole map is described by a **terrain index** (`terrain/index`) the runtime streamer
reads to register the slots and address tiles.

The heightfield may live in a sibling `visual/` folder (`assets/visual/heightfield.png`) instead of
next to the layer file; in that case its `heightfield.json` extents are read from
`assets/visual/heightfield.json` too. A layer file with no resolvable heightfield or layer texture
is left uncooked (a warning, no index). `regia cook --platform android` cooks the same tiled layout
with ASTC 4×4 tiles instead of BCn (ADR-5294). The size rule is the same on both platforms: a
texture slot is a square power of two, at least 256 a side (the tile size); a smaller slot fails the
cook with the tile-size error.

`regia cook --stats` prints the canonical per-map budget and the **actual** tiled sizes per slot
and per map, read back from the cooked cache:

```text
terrain data budget (per map)
  ...
cooked terrain maps:
  assets/textures/terrain.layers.ron
    heightfield         512x512 @256  4 tiles    512.0 KiB
    macro_albedo        BC7 sRGB 512x512 @256  5 tiles   ...
    ...
    total (6 layers)    ...
```

## Budgets

`regia cook --stats` prints the per-map cooked size and the per-profile GPU residency
(`terrain_map_budget`). The numbers are recorded in
`docs/evidence/terrain-data/budgets.md`: two maps cook to ~2.2 GiB (well under the 6 GiB package
target) and the full footprint fits the 5090 and 7900 XTX tile budgets, while Steam Deck Medium
streams against its 1 GiB budget. With `--platform android` the budget counts every slot at ASTC
4×4's 16 bytes per block (`terrain_map_budget_for_platform`), so AO and height count twice their
desktop BC4 size.

## Drawing the cooked map in the runtime frame

The runtime frame and the terrain share one depth buffer, one HDR target, the sun shadow
cascades, and the TAA velocity target. `TerrainSharedDraw` is that hook. It is not a second
image composited on top.

A cooked pak is enough. Mount it, read the terrain index, and build the draw:

```rust
use regia_terrain::gpu::TerrainSharedDraw;
use regia_terrain::stream::PakTileSource;
use regia_terrain::{CdlodConfig, TerrainLayers};

fn attach(
    device: &regia_render::RenderDevice,
    renderer: &mut regia_render::FrameRenderer,
    source: &PakTileSource,
    layers: &TerrainLayers,
) -> Result<(), regia_terrain::TerrainError> {
    let (draw, index) = TerrainSharedDraw::from_cooked(
        device,
        source,
        layers,
        32.0, // world width, used only when the index omits size_m
        1.0,  // height scale
        0.0,  // height offset
        CdlodConfig::default(),
        3, // coarsest CDLOD level
    )?;
    let _ = index;
    renderer.set_shared_draw(Some(Box::new(draw)));
    Ok(())
}
```

`from_cooked` loads `TerrainIndex`, then the tiled heightfield bytes, and stitches them
back to the authored grid (the cooker pads every tile; the stitch drops that padding).
Layer tiles stay on `TileCache` / `PakTileSource` as above. The shared draw writes scene
radiance into the frame's HDR target, so the frame's exposure, aerial fog, and TAA apply
to the terrain. The color pass samples the engine cascades. Motion vectors are
depth-tested, so a closer mesh keeps its velocity. `sky_inscatter` uses the same aerial
in-scatter as a mesh. The offscreen `TerrainGpu` path (sRGB, its own depth) is unchanged and is
what `terrain_lab` still uses.

### Frame cost

A game draws terrain only through `set_shared_draw`. Do not render `TerrainGpu` into a
second target and composite it: that is a second submit and a depth-mask halo. With the
shared draw the frame records into one encoder and submits once.

- Call `draw.prewarm(wgpu::TextureFormat::Rgba16Float)` at map load (the frame's HDR
  format), so the first frame does not compile the four terrain pipelines.
- Keep `let stats = draw.stats();` before boxing the draw. `stats.last_node_upload()` is
  `(writes, bytes)` of node uniforms on the last frame: `(0, 0)` for a held camera, a few
  contiguous ranges while moving. `stats.last_prepare_ms()` is the CPU time of selection
  and upload. `FrameRenderer::shared_shadow_draws()` counts whole-cascade terrain
  draws; it stays flat while the camera and sun are still, and on Desktop High (paged sun
  shadows) a moving camera adds one only when a cascade is placed anew or a cascade's
  terrain list is reselected (after moving about one shadow node, 10–80 m). Pages under
  moved meshes redraw the terrain only inside those pages
  (`FrameRenderer::shared_shadow_region_draws()`).
- After a crater or other height edit, call `draw.update_heights(&map, &dirty)` on the
  draw (not on `TerrainGpu`), so the cached terrain cascades are redrawn.

Depth, color and velocity passes draw only nodes in the camera frustum. Each sun cascade
draws only nodes inside its box, at a level no finer than about two shadow texels per grid
cell. Every pass is one instanced draw (nodes with holes add one draw each). Uploads go
through the renderer's staging belt (`SharedUpload`). To measure a DO27-style chase on the
stand-in maps: `cargo run -p terrain_lab -- --do27-chase` (see `docs/evidence/terrain-perf/`).

## Prepare terrain pipelines before install

Cooked heightfields and textures avoid runtime cooking, but a cold driver may still spend
seconds compiling terrain pipelines. Queue the constructor and configuration before entering
the world. `prepare_terrain` runs that work and the draw's `prewarm` on a map-tier worker:

<!-- ignore-reason: loading-loop fragment with host-owned map, layers, profile, renderer and error callback -->
```rust,ignore
let pending = regia_terrain::prepare_terrain(&warm_queue, color_format, move |device| {
    let mut draw = regia_terrain::TerrainSharedDraw::new(
        device, map, &layers, cdlod, root_level,
    ).map_err(|e| e.to_string())?;
    // Upload cooked materials and set holes, ground look, grass and impostors here.
    draw.gpu_mut().set_virtual_texture_from_budget(&profile.terrain_budget);
    // For a world that uses surface weather:
    draw.gpu_mut().set_warm_surface_weather(true);
    Ok(draw)
});
warm_queue.start(&device); // once, on the device that will render the terrain

// Poll during loading; keep presenting while pending.
if let Some(result) = pending.take() {
    match result {
        Ok(draw) => renderer.set_shared_draw(Some(draw)),
        Err(error) => report_load_error(error),
    }
}
```

`is_finished()` includes errors and cancellation; `is_ready()` means a successful draw is
available. `take()` transfers that result once. The worker owns the terrain's GPU state,
including optional pipelines, so taking and installing it requires no terrain construction,
upload or pipeline creation. When replacing terrain, retire the previous draw outside the
critical frame if releasing its resources is expensive: call `renderer.set_shared_draw(None)`
during loading, before the prepared draw's handoff.

Use the first frame's actual color format and VT policy. The closure can use `from_cooked`
instead of `new`. Set optional features before returning; a later feature, format or device
change can need another prepare. Water-body renderers are separate from this draw. The queue's
worker count and background priority bound concurrent work; a driver compile cannot be
interrupted or promised to finish within a frame budget.

The existing [pipeline warm telemetry](../agents/instant-load.md) reports the `prepare terrain`
map job, its pipelines and errors. Synchronous constructors continue to work; their pipeline
compiles appear in the outside-queue counters and can still block their caller.
