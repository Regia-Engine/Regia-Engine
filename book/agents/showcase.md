# Showcase: a scene directory for filming

`samples/showcase` loads a scene directory and runs it as a headless (or windowed) app with the agent
protocol on, so a person or an agent can place the camera, take screenshots and write a frame-exact
`render.out` sequence of the scene. The scene is a 16-bit heightfield with splat layers, a scatter set
for vegetation and rocks, a sky and weather file, one scripted entity (a stand-in capsule) and a schedule
of VFX and decal spawns. The sample adds no engine feature: it combines the terrain draw, the scatter
placement, the sky and cloud layer, the VFX and decal messages, and `render.out`. Everything is a
function of the frame number of a fixed 60 Hz step, so a scene and a request give the same frames.

The sample has no agent-protocol methods of its own. The existing ones drive it:
[camera paths](camera-paths.md) (`camera.set_path`, `render.sequence`), [render-out](render-out.md)
(`render.out_start`), [screenshots](seeing.md) (`seeing.screenshot`), the pipeline warm
(`render.pipeline_warm`, see [instant load](instant-load.md)) and `quit.request`.

## Run it

```sh
# Write the example scene (procedural; no downloaded assets), then check it loads (no GPU).
cargo run -p showcase -- --write-example /tmp/showcase_scene
cargo run -p showcase -- /tmp/showcase_scene --check

# Headless, agent protocol advertised under /tmp/showcase_project/.regia/agent/.
cargo run -p showcase -- samples/showcase/example_scene --project-dir /tmp/showcase_project

# A window instead (needs a display), the plain look, a smaller scatter mesh.
cargo run -p showcase -- samples/showcase/example_scene --window --plain --scatter-lod 2

cargo run -p showcase -- --list-errors
```

| Flag | Default | Meaning |
|---|---|---|
| `<SCENE_DIR>` | required | The scene directory. |
| `--window` | off | Open a window (the engine's default plugins) instead of running headless. |
| `--fullscreen` | off | Open the window borderless fullscreen on the current monitor (implies `--window`). On a Steam Deck this is the panel's 1280x800, the size the `Deck` render profile is tuned for (the Deck pass in [Steam Deck](../user-guide/steam-deck.md)). |
| `--gpu-timing` | off | Per-pass GPU timestamps in the window (on adapters with timestamp queries), so `telemetry.get_last_summary` reports `gpu_ms` and `gpu_pass_ms`. |
| `--check` | off | Load and validate the scene, print a summary and exit. Needs no GPU. |
| `--size WxH` | 1920x1080 | Size hint stored in the options (the output size is set by `render.out_start`). Mesh LOD switch sizes are scaled to its height (`LodViewport`), so pass the filming size (e.g. `3840x2160`). |
| `--plain` | off | The plain look: no temporal anti-aliasing, no bloom, a fixed exposure (EV100 14.5). The default is the Desktop High film look with auto exposure. |
| `--gpu-driven` | on | The GPU-driven draw path (instanced batches culled on the GPU; repeatable with the order-fixed cull, see "Looks and repeatability"). Kept for older scripts; it is the default since ADR-1415. |
| `--cpu-draw` | off | Draw on the CPU path instead (one draw per instance; ADR-1402's earlier filming default). |
| `--entity-scatter` | off | Draw the scatter as one entity per instance, thinned to `max_scatter_instances` (the path before ADR-1415), instead of the streamed scatter field. |
| `--render-profile NAME` | Desktop High | The render profile preset (`DesktopHigh`, `DesktopUltra`, `Cinematic`, ...; also `--render-profile=NAME`), or a tier with no preset row (`VeryLow`, the integrated-GPU floor, or `Medium`; v0.7, ADR-8844), read with the engine's `take_profile_arg`. With `--demo` and no `--tier`, a tier name is the demo's starting tier. Without it `REGIA_RENDER_PROFILE`, then the scene's `render_profile.txt`, then Desktop High. `Cinematic` runs only on desktop (the startup line says what applied). `render.set_profile` switches it later. `--plain` still pins its own exposure and turns TAA and bloom off on top of the preset. |
| `--grass` | off | Grow terrain grass with the default `grass.ron` even if the scene has none (see "Optional engine features"). |
| `--contact-ao` | off | Bind a contact map to the terrain, fed by the streamed scatter's footprints (see "Optional engine features"). |
| `--foliage-wind` | off | Sway the streamed scatter's glTF trees and cone stand-ins in a 5.4 m/s breeze (`FoliageSway::adopt` on their materials, spec 88 § 2.5); rocks stay still. Not with `--entity-scatter` (see "Optional engine features"). |
| `--ambience` | off | Play the forest ambience bed (`AmbienceBed::forest`, ADR-2442) with generated placeholder loops (shaped noise, not recordings): the wind layers follow the `--foliage-wind` breeze through the bed's wind override, so without it only the quiet air tone plays. Check with `audio.ambience` ([audio ambience](audio-ambience.md)). |
| `--ambient-life` | off | Ambient life (ADR-3024): motes in the sun, insects, butterflies, fireflies, falling leaves by season and a flock and a soaring bird near the camera, by the scene's time and weather (see "Optional engine features"). |
| `--hiz` | off | Keep the GPU cull's Hi-Z occlusion test (GPU-driven path only). |
| `--exposure-ev EV` | scene's | A fixed camera exposure (EV100) overriding the manifest's `camera.exposure_ev`; without either the film look meters automatically. |
| `--scatter-lod N` | 1 | Mesh detail level (0 to 3) of the scatter instances; 0 is the most detailed. |
| `--project-dir DIR` | the working directory | Where the agent endpoint (`.regia/agent/`) is advertised. |
| `--replay-root DIR` | none | A directory `replay.load` may read from on top of the allowed roots (a replay store outside the project; read only, never written). Repeat it for more. Same as `replay_roots` in `.regia/agent.toml` (ADR-1965). |
| `--exit-after-frames N` | 0 | Quit with exit code 0 after N updates (the clock may be paused, so this counts updates). In a window (`--window`, `--fullscreen`) it counts the frames drawn once any loading splash is gone (`showcase::demo::ExitAfterSceneFrames`, ADR-5565; before that the window ignored the flag). `0` runs until `quit.request` or the window is closed. |
| `--write-example DIR` | | Write the example scene into `DIR` and exit. |
| `--list-errors` | | Print the scene error codes and exit. |

Exit codes: 0 done, 2 bad arguments (the usage text is printed), 3 the scene was refused (the line
`showcase: error[<code>]: <file>: <detail>` goes to stderr), 4 no GPU adapter (headless mode creates the
device up front; lavapipe works).

A headless run starts with the game clock paused. `render.out_start` takes the clock for the length of
the job and hands it back, so frames are made as fast as they render.

A headless run also has a headless mixer (no output device, ADR-1561), so `agent.hello` lists
`render_out_audio` and `render_out_audio_stems` and `render.out_start { audio }` writes the WAV mixdown
and stems ([render-out audio](render-out-audio.md)) instead of answering `F0013`.

## The scene directory

```text
my_scene/
  scene.ron                   the manifest
  heightfield.png             16-bit grayscale PNG
  terrain.layers.ron          splat layers (TerrainLayers v1)
  textures/*.png              optional layer textures (8 bits per channel, square, power of two)
  scatter/rocks.scatter.ron   optional scatter set (format v2)
  density/*.png               optional density maps a scatter class names (8- or 16-bit grayscale)
  sky.ron                     sky and weather
  track.ron                   optional scripted track of one entity
  spawns.ron                  optional VFX and decal schedule
  vfx/                        optional *.vfx.ron and *.decal.ron the spawns may name
```

All paths inside the manifest are relative to the scene directory. An absolute path, a `..` component or an
empty path is refused with `scene_manifest_malformed`.

### The cook-convention layout (no `scene.ron`)

A directory without `scene.ron` that holds `terrain/terrain.layers.ron` is read in the cook convention of
`docs/formats/terrain.md` § 9 (the layout a terrain cook and the showcase orchestrator write):

```text
my_scene/
  terrain/terrain.layers.ron   layers (v3); their *_texture paths are relative to terrain/
  terrain/heightfield.png      16-bit grayscale, up to 8192 samples a side
  terrain/heightfield.json     {"size": 8192, "size_m": 8191, "min_m": 27.0, "max_m": 421.3, ...}
  terrain/detail/<name>/{albedo,normal,orm,height}.png
  terrain/macro/pack0.png      optional splat packs (both or neither)
  terrain/macro/pack1.png
  scatter/*.scatter.ron        optional; the first by name. Meshes beside it (scatter/meshes/<id>/<id>.gltf)
  sky.ron                      required
  track.ron, spawns.ron, vfx/  optional
```

`heightfield.json` maps onto the manifest: `size_m` is `world_width` (the distance from the first to the last
sample, so 8192 samples at 1 m cells is 8191), `min_m` is `height_offset`, `max_m - min_m` is `height_scale`
(sample 65535 is `max_m`), and `size`, when present, must match the PNG. Unknown fields (`origin`, notes) are
ignored: the map is centered on the world origin. Every layer that names an `albedo_texture` becomes a `Files`
material layer (all layers or none); `macro_albedo.png` and `macro/{normal,ao}.png` are not used yet. A
`scene.ron` next to `terrain/` wins, so an existing manifest scene keeps loading as before. Nothing in the loader
assumes a map size: heightfield, splat packs, density maps and the scatter placement all follow the files.

### `scene.ron` (version 1)

| Field | Default | Meaning |
|---|---|---|
| `version` | required | Must be `1`. |
| `name` | empty | A label for reports. |
| `heightfield` | required | The PNG. Row 0 is at -Z, column 0 at -X; the map is centered on the origin. |
| `world_width` | required | Width and depth of the map in metres (finite, > 0). |
| `height_scale` | required | Height of sample value 65535 above `height_offset`, metres (finite, > 0). |
| `height_offset` | `0.0` | Height of sample value 0, metres. |
| `heightfield_size` | none | Samples a side the PNG must have (checked). |
| `layers` | required | The `.terrain.layers.ron`. |
| `material` | none | Per-layer textures (below). Without it the layers' flat colors are used. |
| `splat` | none | Splat weight packs: `Some((pack0: "macro/pack0.png", pack1: "macro/pack1.png"))` (below). |
| `scatter` | none | The `.scatter.ron`. |
| `stand_ins` | none | Which stand-in mesh each scatter mesh name gets (below). |
| `max_scatter_instances` | 20000 | Cap on scatter instances turned into entities with `--entity-scatter`; extra instances are thinned evenly. The streamed field (the default) draws every instance. |
| `sky` | required | The sky and weather file. |
| `track` | none | The track file. |
| `spawns` | none | The spawn file. |
| `vfx_dir` | none | A directory of effect and decal prefab files. |
| `camera` | see below | The starting camera: `position`, `target`, `fov_deg`, `near`, `far`, `exposure_ev`. The agent takes it over. |
| `lead_in_frames` | 0 | The scripted clock starts this many frames before frame 0 (see "The clock"). |

Unknown fields are refused, so a typo is an error instead of a silently ignored setting.

### Heightfield, layers and material

The heightfield must be a grayscale PNG with 16 bits per sample, from 2x2 to 8192x8192 samples. The terrain
draw is the engine's shared terrain pass (CDLOD, splat layers by slope and altitude), the same one a game
uses. The layers file is the engine's `TerrainLayers` format (`docs/formats/terrain.md`).

`material` adds photoreal layers: one entry per layer of the stack, in order. An entry is either
`Style(style: "Grass", size: 256, seed: 31)` (a procedurally baked tiling set; `style` is `Grass`, `Rock`,
`Dirt`, `Snow`, `Sand` or `Ice`, `size` a power of two from 64 to 2048) or
`Files(albedo: "textures/a.png", normal: Some(...), height: Some(...), orm: Some(...))`. Texture files are
8 bits per channel, square, a power of two, and the same size for every layer. A missing normal or height
map is flat; a missing ORM map keeps the layer's scalar roughness. Layer textures may be up to 4096 a side and
are decoded on up to eight threads.

`material.relief` sets the procedural detail the engine's photoreal terrain path draws over the textures
(`regia_terrain::TerrainRelief`). Fields: `near_detail`, `mid_relief`, `strata`, `rock_detail`, `talus`,
`self_shadow_reach_m`, `cavity`, `glints`, `bounce`, `cliff_relief`, `layer_occlusion`. Absent, it is the
**photo-texture preset**: near-field grain and pebbles, mid-range rock-and-pit relief, strata and the
fractured-rock normal off; self-shadow, cavity, bounce, glints and cliff geometry as the engine's defaults. The
engine's own defaults are tuned for bare planetary ground; over photographed grass and leaf litter their
pebbles, grain ripples and micro shadows hide the texture's millimetre detail (the "smeared blobs" of the first
showcase frames). To get the planetary look back, write the engine values, e.g.
`relief: (near_detail: 1.0, mid_relief: 1.0, strata: 0.85, rock_detail: 1.2)`.

`splat` (or `terrain/macro/pack*.png` in the cook layout) paints the layers per texel: `pack0` holds the weights
of layers 0 to 3 in R, G, B, A and `pack1` those of layers 4 to 7, both RGBA8 PNGs of the same size (up to
8192) over the whole map, column 0 at -X and row 0 at -Z. A layer's weight is its pack weight times its slope
and altitude band (open bands leave the pack as authored); where the packs hold no weight at all the bands
decide. This is the engine's `TerrainSharedDraw::set_splat_weights` (ADR-1402).

### Scatter

The set is the engine's `.scatter.ron` format (v2) and is placed deterministically over the heightfield
(slope, altitude, layer, density map and exclusion rules apply). By default the whole set is drawn through the
engine's streamed scatter field on the ECS frame ([streamed forests](scatter-forest.md), ADR-1415): tiles are
placed around the camera, each instance picks its LOD with a dithered cross-fade (a tree's billboard at LOD 3),
fades out at its class's `fade_start..fade_end`, casts from its shadow LOD, and is seated on the drawn terrain;
one shared material per mesh and part keeps each texture resident once. `terrain.scatter` reports the field.
`--entity-scatter` restores the older path: each placed instance becomes one entity (one per material part for
a glTF mesh, far-only parts such as billboards left out) sharing its mesh and material with the others of the
same mesh name, class and part, thinned evenly to `max_scatter_instances`.

A mesh name ending in `.gltf` or `.glb` is a glTF file, looked up beside the `.scatter.ron` first and then in
the scene directory (`meshes/boulder_01/boulder_01.gltf`). The reader takes `.gltf` with external `.bin`
buffers and `.glb`, applies the node transforms (the 1 m normalization on a root node), reads PNG and JPG maps
(base color, normal, packed ORM / ARM, emissive; capped at 1024 a side), alpha mask and double-sided flags,
and skips nodes named `billboard` unless they name their level (a tree's `<stem>_LOD3` billboard is a level).
Nodes named `<name>_LOD<n>` are the mesh's LOD levels, `extras.lod_screen_size` on a level node sets its switch
size for the streamed field and `extras.shadow_lod` its shadow LOD; a mesh with only
LOD 0 gets two vertex-clustered levels. The levels become the engine's `MeshLodChain`, switched by projected
size (0.12, 0.035, 0.01 of the view height). A missing glTF is `scatter_mesh_missing`, an unreadable one
`scatter_mesh_malformed`.

`stand_ins` binds a name to a procedural stand-in instead (it wins over a glTF file of that name):
`Rock(color: (r, g, b))` (an angular rock, 1 m across at scale 1), `Boulder(color: ...)` (a rounded one) or
`Cone(color: ...)` (1 m tall, for conifers). Any other name with no binding gets a gray rock. Colors are
linear. To film the real meshes, remove their `stand_ins` entries.

### `sky.ron` (version 1)

```text
Sky(
    version: 1,
    atmosphere: (time_of_day: Some((hour: 8.0, day_of_year: 80, latitude_deg: 46.0))),
    look: (exposure_bias_ev: 0.0),
    weather: Preset("fair"),
    changes: [(frame: 120, to: Preset("cloudy"), seconds: 6.0)],
)
```

`atmosphere` is the engine's `LevelAtmosphere` (planet preset, sun clock, haze, aerosol turbidity, sky
bodies) and `look` its `LevelLook` (exposure bias, grade), applied the way a level's are. The sun clock is
fixed for the run. `weather` is `Preset("clear" | "fair" | "cloudy" | "overcast" | "storm")` or
`Custom(coverage: .., cloud_type: .., density: .., wind_m_per_s: (x, z), precipitation: ..)`. Each entry in
`changes` eases the weather to another one over `seconds` (`0` jumps) starting on `frame`.

The showcase leaves the cloud layer's three map switches at the engine default, off (see [clouds](clouds.md)): an
overcast sky keeps the trees' hard sun shadows (ADR-5419). `render.set_clouds { "shadow_map": true, "fog_coupling":
true, "overcast_ibl": true }` takes the direct sun away under a cloud for a still or a comparison, but the aerial
haze is not shadowed by the layer, so views from high up wash out under a closed cover (ADR-5792): that is why the
host does not switch them on.

### `cloud_look:` in `sky.ron`

```text
Sky(
    version: 1,
    weather: Preset("cirrus"),
    cloud_look: Some((cirrus_coverage: Some(0.65), cirrus_optical_depth: Some(0.11), cirrus_scale_m: Some(1500.0))),
)
```

Optional (ADR-1492). The weather step rewrites the cloud layer's look from the weather's genus every fixed
step, so a genus preset such as `cirrus` always shows its own sheet. `cloud_look` pins values over it: each
field is `Some(number)` or left out, from `diffusion`, `erosion`, `breakup`, `breakup_scale_m`,
`cirrus_coverage`, `cirrus_altitude_m`, `cirrus_optical_depth`, `cirrus_scale_m` and `cirrus_stretch`. The
values are clamped to the same ranges as `render.set_clouds`; a number that is not finite or an unknown field is
`sky_malformed`. The pins hold through timed `changes`. A file without the block draws exactly as before, and
the format stays `version: 1` (an added optional field). Agents can change the pins at run time with
`render.set_clouds` and drop them with `clear_look_override` ([clouds](clouds.md)).

### `track.ron` (version 1): the scripted entity

```text
Track(
    version: 1,
    entity: "pod",
    capsule: (radius: 0.9, half_length: 1.2),
    color: (0.55, 0.57, 0.6),
    height_mode: AboveGround,
    interpolation: CatmullRom,
    // Optional: a glTF instead of the capsule, and the damaged variant it swaps to.
    // mesh: Some("meshes/hardware.gltf"),
    // swap_mesh: Some("meshes/hardware_damaged.gltf"),
    // swap_frame: Some(420),
    keys: [
        (frame: 0,   position: (-200.0, 160.0, -420.0), euler_deg: (-35.0, 35.0, 0.0)),
        (frame: 150, position: (-8.0, 14.0, -112.0),    euler_deg: (-12.0, 20.0, 0.0)),
        (frame: 180, position: (0.0, 2.1, -100.0),      euler_deg: (-4.0, 15.0, 6.0)),
    ],
)
```

The entity is a capsule standing along its local Y axis, a placeholder for a pod. `euler_deg` is
(pitch about X, yaw about Y, roll about Z), applied yaw, then pitch, then roll. Keys are placed by frame
number and the frames must strictly increase. Before the first key the pose is the first key's; after the
last it is the last key's. `Linear` draws straight lines between positions and `CatmullRom` a uniform
Catmull-Rom spline through them (end keys repeat); rotations always slerp. With `height_mode:
AboveGround`, `position.y` is metres above the terrain surface under the entity; with `Absolute` it is a
world height. The entity name is 1 to 64 characters of `a-z`, `0-9`, `_` and `-`. The entity carries a
`Tracked(name)` component and its `Transform` is rewritten every fixed step.

**A glTF instead of the capsule.** `mesh` names a `.gltf` or `.glb` file, relative to the scene directory (not
absolute, no `..`), read by the same loader as the scatter meshes: the file's own materials and its `doubleSided`
flag are kept, the node transforms are baked in, the origin of the file is the track position, and the entity is one
`Tracked` entity per material part with a LOD chain (a file with one level gets two coarser ones by vertex
clustering). `capsule` and `color` are then unused. Without `mesh` the capsule is drawn as before.

**The swap.** `swap_mesh` (a glTF as above) and `swap_frame` go together. On the first frame at or after
`swap_frame` the director despawns the tracked entities and spawns the swap mesh's parts at the same pose, which
keeps following the keys; on a frame before it (a seek back) the first mesh returns, so the swap is a pure
function of the frame clock. `swap_frame` is a track frame, the same unit as the keys. The swap does not use the
engine's `DamageSwap` (`regia_vfx`): that one is driven by the debris clock, replaces one mesh and material on one
entity and pins the pose of the wreck, which a multi-part glTF on a scripted track cannot use (ADR-1426).
`ShowcaseStats` reports `track_parts` and `track_swapped`.

### `spawns.ron` (version 1): effects and decals at given frames

```text
Spawns(
    version: 1,
    effects: [
        (frame: 180, effect: "explosion", at: UnderEntity(offset: (0.0, 1.0, 0.0)), forward: (0.0, 1.0, 0.0), seed: Some(7)),
    ],
    decals: [
        (frame: 180, at: UnderEntity(offset: (0.0, 0.0, 0.0)), prefab: Some("scorch"), half_size: (6.0, 6.0, 2.0), yaw_deg: 20.0),
        (frame: 181, at: Ground(x: 4.0, z: -96.0, offset: 0.0), color: (0.02, 0.018, 0.016, 0.8), half_size: (9.0, 9.0, 3.0)),
    ],
)
```

`at` is where the spawn happens: `World((x, y, z))`; `Ground(x:, z:, offset:)` (the terrain surface
there plus `offset`); `Entity(offset: (x, y, z))` (the tracked entity's position on that frame plus a
world-space offset); or `UnderEntity(offset: ..)` (the terrain surface under the entity, plus `offset`).
The last two need a track (`spawn_needs_track` otherwise).

- An effect is a `SpawnEffect` message (`regia_vfx`). `effect` names an engine effect
  (`explosion`, `smoke`, `dust_trail`, `impact`, `sparks`, `weather`, `exhaust`, `tracer`, `muzzle_flash` and the
  other built-ins) or one loaded from `vfx_dir`. `forward` is the effect's local +Z (default up). `seed` makes
  the effect repeatable; without it the engine derives one from a per-run counter.
- A decal is a `SpawnDecal` message, or with `prefab` a named decal prefab placed with `place_decal`
  (`scorch`, `pad_marking`, `rut`, `grime` or one from `vfx_dir`). `normal` defaults to the terrain normal for
  a ground place and world up otherwise. `half_size` and `color` apply to the plain form, `lifetime` and
  `yaw_deg` to both.

Every entry fires exactly once, on its frame, in the order effects then decals within a frame and file order
within each.

## The clock

In a window (`--window`, `--fullscreen`) the clock follows the wall clock: the 60 Hz fixed step runs as many
steps as a frame's time holds, so the track plays at its real speed under any frame cap (`loop.set { max_fps }`)
and telemetry's frame times are wall time. Before v0.2.6 the window kept the headless app's manual 1/60 s clock:
below 60 fps the track slowed down and telemetry reported 16.67 ms for every frame (found in the Steam Deck pass,
ADR-1750). The rest of this section is about the headless app and render-out jobs.

The director runs in the 60 Hz fixed step. The frame counter starts at `-lead_in_frames` and counts fixed
steps; while the clock is paused it does not move. A `render.out_start` job unpauses the clock, and its
`warmup_frames` also step the clock before the first delivered frame. To have delivered frame 0 be track
frame 0, set `lead_in_frames` in the manifest to the `warmup_frames` you pass (or use `warmup_frames: 0`
with `lead_in_frames: 0`).

The director's counter is the scene's state, not the job's: a second job in one host continues from where the
first left it and renders later frames of the timeline, so its frame hashes differ from the first job's. Two fresh
hosts repeat each other ([what repeats](render-out.md#what-repeats)).

The director runs once per 60 Hz step of game time whatever the render-out frame rate: at `fps: 10` each frame runs
six steps and the track plays at its real speed (a frame count of `6 * n + warmup` in 60 Hz steps), at `fps: 30`
two. Before ADR-1426 the host kept the engine's one-step-per-frame cap, so any rate below 60 played the track, the
spawns, the weather changes, the grass sway and the entry streak at `fps / 60` of their speed while the particle
effects ran in real time; a capsule that fell in 7 s of game time was still above the frame at the end of a 10 fps
job. Rendering is the same at any rate. A step is not a whole number of nanoseconds, so the first frame of a job
can run one step fewer than the rate says.

## Driving it

```json
{"method": "render.pipeline_warm", "params": {}}
{"method": "camera.set_path", "params": {"path": {"interp": "CatmullRom", "keys": [
  {"time": 0, "position": [60, 46, -170]}, {"time": 4, "position": [20, 40, -130]}],
  "look_at": {"Point": [0, 36, -100]}}, "fov": [{"time": 0, "value": 45}], "play": true}}
{"method": "render.out_start", "params": {"out": "/tmp/showcase_out", "name": "shot", "frames": 240,
  "width": 3840, "height": 2160, "fps": 60, "warmup_frames": 0}}
{"method": "render.out_progress", "params": {"hashes": true}}
{"method": "seeing.screenshot", "params": {"target": "offscreen"}}
{"method": "quit.request", "params": {}}
```

Wait for `render.pipeline_warm` to report `boot_background.idle: true` before the first job. The camera path
plays on the game clock, so a render-out job moves the camera one fixed step per frame. The agent's
`output_roots` setting (`.regia/agent.toml`) decides which directories `out` may name.

### Looks and repeatability

The default is the Desktop High film look (temporal anti-aliasing, bloom, auto exposure). `--plain` turns
those off and fixes the exposure (EV100 14.5). Two renders of one sequence from fresh hosts on one GPU should give the same
frame hashes in either look; hashes from different GPUs or drivers differ. Measured on an RTX 5090: the film
look, 120 frames at 1920x1080, gave the same sequence hash in two runs of one process and in two separate
processes. The sample sets these renderer switches for this (ADR-1402, ADR-1415):

- **Order-fixed GPU cull** (`FrameRenderer::set_ordered_gpu_cull`, on by default since ADR-4777; ADR-1415). The
  GPU cull used to append each batch's visible instances with an atomic counter, so their draw order changed
  from run to run, and with temporal anti-aliasing at 1920x1080 that order showed in the frames (ADR-1402
  therefore drew on the CPU path). Every instance keeps the slot of its submission order, so the GPU-driven
  path (the default again; the streamed forest needs it) is repeatable. `--cpu-draw` still selects the CPU
  path.
- **Twig cards** (`set_taa_hashed_alpha(true)`, `set_alpha_tested_shadows(true)`, ADR-1415): masked materials take
  the hashed alpha test under temporal AA (stable coverage instead of a shimmering hard edge) and cast
  alpha-tested shadows on the GPU-driven path.
- **Hi-Z occlusion off** (`FrameRenderer::set_hiz_occlusion(false)`; `--hiz` turns it back on). The GPU-driven
  cull's occlusion test reads a depth pyramid built from whatever the size's depth target last held, and on an
  RTX 5090 it was the one pass whose result varied between two identical runs (bisected one feature at a time).
  With it off every instance in the frustum is drawn; on the 8192 m showcase map that was not slower.
  This one matters when `--gpu-driven` is on.
- **Exposure readback waits** (`FrameRenderer::set_wait_for_exposure_readback(true)`). Auto exposure meters a
  histogram two frames late; without the wait each sample joins the meter on whichever frame its readback
  lands, which differs from run to run. A render-out job (`render.out_start`) turns the wait on for its own
  frames and restores it afterwards. Live play leaves it off (ADR-4050): the wait drains the whole GPU queue, so
  on a GPU-bound device it stalled every second frame (`play.hitch_report` counted them as blocking polls: 429
  in 60 s at Desktop Low on an Intel N150 before the change, 0 after).

### Filming settings

- **Fixed exposure.** A fixed `Camera.exposure_ev` turns auto exposure off for that camera. The sample sets it
  from, in order: `--exposure-ev EV`, the manifest's `camera.exposure_ev`, or `sky.ron` when its meter range is
  one value (`look: (min_ev100: Some(13.5), max_ev100: Some(13.5))`). At run time an agent sets it with
  `scene.set_property` on the camera entity: `{"entity": <camera>, "component": "Camera", "path":
  "exposure_ev", "value": 13.5}` (`null` returns to auto exposure). `scene.get_entity` shows the current value
  in the same JSON shape; check it there before a long render.
- **Depth of field keys.** In `camera.set_path` (and `.shots.ron`) `dof` is
  `{"focus_distance": [keys], "aperture": [keys], "focus_on_target": false, "bokeh": ...}`. Each key is
  `{"time": seconds, "value": v, "ease": "Linear" | "Step" | "Smooth"}`: `ease` sits on the key and shapes the
  move from that key to the next (`Linear` when absent). Example:
  `"dof": {"focus_distance": [{"time": 0, "value": 40, "ease": "Smooth"}, {"time": 6, "value": 8}],
  "aperture": [{"time": 0, "value": 2.8}]}`.
- **LOD size.** Pass the filming size with `--size` (e.g. `--size 3840x2160`) so mesh LOD switches match it.

## Errors

A scene that cannot be loaded is refused before any window or device exists, with one of these codes in
`showcase: error[<code>]: <file>: <detail>` on stderr and exit code 3. Codes are lowercase ASCII, digits and
`_`.

| Code | Meaning |
|---|---|
| `scene_dir_missing` | The path does not exist or is not a directory. |
| `scene_manifest_missing` | The directory has neither a `scene.ron` nor a `terrain/terrain.layers.ron`. |
| `scene_manifest_malformed` | `scene.ron` does not parse, has the wrong version, an unknown field, a path that leaves the directory, or a number out of range. |
| `scene_file_missing` | A file the manifest names does not exist (in the cook layout: `heightfield.json`, `sky.ron`, a layer texture, or one splat pack without the other). |
| `heightfield_malformed` | The heightfield is not a readable 16-bit grayscale PNG of 2 to 8192 samples a side, its mapping is invalid, `heightfield.json` is malformed (`size_m` > 0, `max_m` > `min_m`), or the PNG is not the size the scene says. |
| `layers_malformed` | The layers file does not parse or fails validation. |
| `materials_malformed` | The `material` block has the wrong number of layers, an unknown style, a bad size or an unreadable or mismatched texture. |
| `scatter_malformed` | The scatter set (or a density map it names) is invalid. |
| `sky_malformed` | The sky file does not parse, has an unknown weather preset or a value out of range. |
| `track_malformed` | The track file does not parse, has no keys, non-increasing frames or a bad value. |
| `track_mesh_missing` | The track's `mesh` or `swap_mesh` names a file that is not in the scene directory. |
| `track_mesh_malformed` | The track's `mesh` or `swap_mesh` cannot be read as a glTF (bad JSON or GLB, a buffer or accessor out of range) or has no triangles. A name that is not a `.gltf` or `.glb`, a path that leaves the scene directory, or a `swap_mesh` without a `swap_frame` is `track_malformed`. |
| `spawns_malformed` | The spawn file does not parse or holds a bad entry. |
| `spawn_unknown_effect` | A spawn names an effect that is not built in and not in `vfx_dir`. |
| `spawn_unknown_decal` | A spawn names a decal prefab that is not built in and not in `vfx_dir`. |
| `spawn_needs_track` | A spawn uses `Entity` or `UnderEntity` but the scene has no track. |
| `splat_malformed` | A splat pack is not a readable PNG of at most 8192 a side, or the two packs differ in size. |
| `scatter_mesh_missing` | A scatter mesh name ends in `.gltf` or `.glb` but the file is neither beside the `.scatter.ron` nor in the scene directory. |
| `scatter_mesh_malformed` | A scatter glTF cannot be read (bad JSON or GLB, a buffer or accessor out of range, an undecodable map) or has no triangles. |
| `grass_malformed` | `terrain/grass.ron` does not parse, has the wrong version or a `texel_m` outside 0.5 to 16, names a terrain layer that does not exist, or its set fails the grass validation or bake. |
| `water_malformed` | `terrain/stream.json` does not parse or describes a ribbon the water draw refuses. |
| `entry_malformed` | `entry.ron` does not parse, has fewer than two points, times that do not strictly increase, a non-finite value or a negative knob. |
| `tunnels_malformed` | `tunnels/tunnels.ron`, its mesh-layers document or a passages file does not parse or breaks a rule ([Showcase tunnels](showcase-tunnels.md)). |
| `tunnels_ground_mismatch` | The heightfield is not the square ground lattice `tunnels.ron` describes: not square, or a sample off its ground counts. |
| `tunnels_cook_failed` | The mesh-layers cook refused the tunnels; the message carries the cook's own code. |

## Optional engine features

Each feature is a file or a flag and is off without it, so a scene written before the feature existed
loads and renders as before. All of them are built at load time on the CPU (`--check` validates them with
no GPU), run on the 60 Hz frame clock, and report a stable error code when their file is bad.

| Feature | Switch | What the host does | Agent surface |
|---|---|---|---|
| Terrain grass | `terrain/grass.ron`, or `--grass` | Bakes a `GrassMask` from the splat packs and binds a `GrassLayer` to the terrain draw; sets the sway clock each step | `terrain.grass`, `terrain.set_grass` ([terrain grass](terrain-grass.md)) |
| Valley mist | a `mist:` block in `sky.ron` | Samples a `MistHeightGrid` from the loaded heightfield (`mist_grid_from_map`) and inserts the `ValleyMist` resource; sets `time_s` from the frame | `render.get_valley_mist`, `render.set_valley_mist` ([valley mist](valley-mist.md)) |
| Contact AO | `--contact-ao` | The streamed scatter field does not bind a contact map by itself; the host creates a `ContactMap`, binds it to the terrain draw and rasterizes the field's footprints under the camera every frame (`Render`, after the scatter extraction). Needs the streamed field, so it does nothing with `--entity-scatter` | `terrain.scatter` (`contact`) |
| Water | `terrain/stream.json` | Spawns one `WaterBody` ribbon (points in source order) and adds `WaterPlugin` | `water.state`, `water.set`, `water.probe` ([water](water.md)) |
| Entry streak | `entry.ron` | Adds `EntryStreakPlugin`, spawns one `EntryStreak` and sets its `frame` from the clock | `vfx.entry_state` ([entry streak](entry-streak.md)) |
| Tree sway | `--foliage-wind` | Adopts the materials of every glTF scatter mesh and cone stand-in with `FoliageSway::adopt` at the plant's height (`showcase::app::swaying_library`), builds the streamed field from that library and sets the renderer's scene wind (`SHOWCASE_WIND`); `ShowcaseStats::swaying_materials` counts the adopted materials. Streamed field only | `render.foliage_wind`, `render.set_foliage_wind`, `material.sway`, `material.set_sway` ([foliage wind](foliage-wind.md)) |
| Ambient life | `--ambient-life` | Inserts `AmbientLifeSettings { enabled: true }` (`VfxPlugin` adds the systems); with no placed flock it puts a flock and a soaring bird near the camera. Off on Low and below | `render.ambient_life`, `render.set_ambient_life`, `render.set_ambient_birds` ([ambient life](ambient-life.md)) |
| Render profile | `--render-profile NAME` | `RenderSettings::from_profile` of the (gated) preset | `render.get_profile`, `render.set_profile` ([render profile](render-profile.md), [Cinematic](cinematic.md)) |
| Tunnels | `tunnels/tunnels.ron` | Cooks the mesh-layers document on the heightfield at load, cuts the mouths through the terrain draw (named holes drawn from the cooked lattice cut), keeps scatter out of them, spawns the tube meshes and lights and inserts the cooked `VisCells` (ADR-8825) | `tunnels.state`, `tunnels.view` ([Showcase tunnels](showcase-tunnels.md)) |

### `terrain/grass.ron` (version 1)

```text
Grass(
    version: 1,
    dead_grass: "dead_grass",   // terrain layer the dead stems grow on (pack0 G in the showcase scene)
    shoots: "shoots",           // terrain layer the green shoots grow on (pack0 B)
    texel_m: 2.0,               // mask texel edge, 0.5 to 16 m
    seed: 0,
    set: None,                  // Some((version: 1, classes: [...])): a whole grass set, names then unused
)
```

Every field but `version` is optional. The two names are looked up in the layer file; a name the terrain
does not have is `grass_malformed` and the message lists the layers there are. The default set is one class of
dead winter grass (`GrassClass::winter_dead`) that grows where the `dead_grass` layer's weight is, with green
shoots (shorter, mostly green) where the `shoots` weight is. The weights are the splat packs the terrain draws:
layer `k` is channel `k % 4` of pack `k / 4`, laid over the procedural slope and altitude bands exactly as the
terrain layers blend painted weights. Without splat packs only the bands decide. After a frame,
`terrain.grass` reports `installed: true`, and `drawing: true` once clumps are in range. The bake is a pure
function of the heightfield, the layers and the packs.

### `mist:` in `sky.ron`

```text
Sky(
    version: 1,
    ...
    mist: Some((density: 0.012, max_height_m: 45.0, grid_cell_m: 32.0, concavity_radius_m: 300.0)),
)
```

Every field is optional and defaults to the engine's `ValleyMist` (`grid_cell_m` 32, `concavity_radius_m` 300).
The grid is sampled from the scene's heightfield, so the mist pools in the valleys of the loaded map. Its
`time_s` is the frame clock (`frame / 60`, zero before frame 0). A bad number is `sky_malformed`.

### `terrain/stream.json`

```text
{ "points": [ { "x": 1099.5, "z": -6.2, "water_y": 123.2, "width_m": 1.4, "depth_m": 0.12 }, ... ] }
```

Points run from the source downstream; other keys (the orchestrator's `flows` note) are ignored. The ribbon
uses `WaterLook::clear_stream()` and Manning flow from the slope. `water.set { tier: "ultra" }` gives the
costliest look for offline renders. A file the water draw refuses (fewer than two points, a coincident pair,
a width or depth out of range) is `water_malformed`.

### `entry.ron` (version 1)

```text
Entry(
    version: 1,
    points: [(time: 0.0, position: (90000.0, 80000.0, 107000.0)), (time: 120.0, position: (700.0, 105.7, 5.0))],
    start_frame: 0,     // the clock frame at which track time 0 plays
    tier: Full,         // or Cheap
    shed_count: Some(4),
)
```

The streak shows track time `(frame - start_frame) / 60` seconds, so seeking and re-rendering give the same
picture (frames before `start_frame` show time 0). Optional profile knobs: `ground_y` (default: the map's lowest
ground), `seed`, `peak_radiance`, `head_radius_m`, `body_radius_m`, `ion_life_s`, `train_life_s`, `wind`,
`shed_altitude_m`, `shed_count`, `damage_smoke`, `min_px`. A bad track or a negative knob is `entry_malformed`.

### Camera shake and crash lights

The host installs the shock-shake schedule (`ShockShakePlugin`) and applies the camera director
itself, after the shot player: `camera.schedule_shock_shake { distance_m: 3300 }` queues the shake
of a blast 3.3 km away, which reaches the camera `distance / 343` s later (578 steps at 60 Hz) and
decays over 3.5 s. The shake is added to whatever pose the shot player or `camera.set_*` wrote, so
it is a pure function of the frame clock; `camera.shock_shake` lists the schedule and what has
fired. `vfx.crash` also lights the fires now: a light at every visible ground fire, same positions
and seeds (`"fire_lights": false` to leave them out; see `agents/crash-fx`). Next to it,
`vfx.spawn_crash_fires` with `count` 0 adds only the wreck glow.

### Render profile and size

`--render-profile Cinematic --size 3840x2160` is the offline filming setup: the preset sets the cloud march, the
cascades and the light caps, and `--size` scales the mesh LOD switch sizes to the filming height. The startup
line `showcase: render profile <name> (<source>)` says what applied and why (a requested Cinematic on Android
or a lower tier is replaced by the engine's gate).

## The example scene

`samples/showcase/example_scene` is generated by `showcase::example` from fixed numbers: a value-noise
heightfield with a ridge and a flat landing area, three layers (grass, rock, snow) with procedurally baked
textures, a scatter set of rocks and pines with stand-in meshes, a morning sky that clouds over, a capsule
that descends and lands, and a schedule that fires a dust trail, an explosion, smoke and two decals at the
landing (the scorch prefab and, inside its footprint, a plain tinted box as its dark core). It contains no
downloaded or third-party assets. A test regenerates it and compares every file with the committed copy, so
edit `src/example.rs` and run `--write-example` instead of editing the files.

`example_scene/cinematics/` is the exception: hand-written and not generated (the generator neither writes nor
removes it). `intro.seq.ron` is a `.seq.ron` v2 sequence with every track kind (keyed vec3, float and bool
tracks with Bezier keys, a clip crossfade, two sounds and a sparkle sub-sequence played twice, the second time
with an offset and at 1.5x, which holds a twinkle of its own), and `intro.render_job.ron` renders it through
render-out with a binding table (ADR-2135). The showcase app does not play it by itself, so the default frames
do not change; a render job or the editor plays it:

```json
{ "method": "render.out_job", "params": { "file": "cinematics/intro.render_job.ron", "dry_run": true } }
{ "method": "editor.seq_curve_open", "params": { "path": "cinematics/intro.seq.ron" } }
```

A scene without a `beacon` entity refuses the job with `render_job_unbound`; the plan lists the targets the
table leaves out (`keeper`, `sun`) under `unbound_targets`.

## Checks

- `tests/scene.rs`: the example loads with every part; each kind of bad scene is refused with its code
  (missing directory, missing manifest, malformed manifest, missing and malformed files, 8-bit and truncated
  heightfields, unknown effects and decal prefabs, entity spawns without a track, bad materials); the
  committed example equals its generator; placement is deterministic, thinned evenly and clear of the
  landing; every error code is documented on this page.
- Unit tests in `src/track.rs` (interpolation, holds, slerp, parsing), `src/spawns.rs` (sorting, each event
  exactly once, place resolution), `src/sky.rs` and `src/cli.rs`.
- `tests/sequence.rs` (no GPU): the intro reads its two-level sub-sequence tree from disk, uses every v2 track
  kind, samples the same bits on every run (a pinned hash of 181 frames), plays on named entities through the
  sequencer, and its render job binds only targets the intro has. `regia_agent/tests/render_out_showcase_sequence.rs`
  (lavapipe) renders it through the job twice with identical bytes.
- `tests/director.rs` (no GPU): the pod follows the track frame by frame, effects and decals fire once on
  their frames, the weather change applies, two runs agree record for record.
- `tests/track_mesh.rs` (no GPU): a track's glTF loads beside the scene (one part, 12 triangles) and the capsule stays
  the default; a missing file, a garbage file and a missing swap file have their codes and the file at fault; a wrong
  extension, a swap without its frame and paths that leave the directory are `track_malformed`; the director swaps
  the entity on its swap frame and not before, and the swapped entity keeps following the track; a 100 ms frame runs
  six director steps (and one without the cap, the old behavior).
- `tests/track_draw.rs` (lavapipe works): the tracked capsule changes the frame inside its box against a frame
  without the track on the old path, the GPU-driven path and with Hi-Z; two renders are hash-identical; a falling
  track at `fps: 10` reaches its rest pose; a glTF mesh is drawn instead of the capsule; the swap shows the damaged
  mesh after its frame and the same frame as the unswapped scene before it.
- `tests/render.rs` (a Vulkan adapter, lavapipe works): 8 frames at 640x360 through `render.out_start`, twice,
  with identical hashes. It prints a note and passes when there is no adapter.
- `tests/cook_layout.rs` (no GPU): the cook layout loads with its extents, layers and splat packs; a `scene.ron`
  wins; each malformed variant has its code; an 8192 x 8192 heightfield loads at 1 m cells.
- `tests/texture_detail.rs` (lavapipe): a 4 mm checker survives at 1 m, the photo relief preset draws nothing over
  a flat layer while the engine preset does; splat packs paint each half of a map with its own layer.
- `tests/scene.rs` also loads a glTF scatter mesh beside the `.scatter.ron` (root-node scale applied), lets a
  stand-in binding win, and refuses missing and malformed glTF files.
- `tests/features.rs` (no GPU): a procedural scene with every feature on loads and bakes what each needs (grass
  grows on the painted layer and nowhere else, the bake is repeatable, the mist has a grid from the heightfield,
  the stream is one 40-point ribbon, the entry streak follows the clock); a scene with every feature file absent
  has no feature resource, plugin or entity; each malformed file has its code.
- `tests/features_render.rs` (a Vulkan adapter): the feature scene renders 8 frames twice with identical hashes
  with all features bound (`terrain.grass`, `water.state` and `render.get_valley_mist` answer), and switching
  grass, mist and water off over the agent protocol changes the frames.
- `tests/features_acceptance.rs` (ignored; a real GPU and the orchestrator's scene): a frame-time table with one
  run per feature and two identical all-on runs.
- `tests/acceptance.rs` (ignored; a real GPU): 120 frames twice with identical hashes, the same in two processes
  (`one_run_of_120_frames_writes_its_hashes`, `SHOWCASE_HASH_OUT`), the cost of a scene at 2560x1440 and 3840x2160
  (`frame_cost_at_1440p_and_4k`; `SHOWCASE_TEST_SCENE`, `SHOWCASE_ACCEPT_CAMERA=ex,ey,ez,tx,ty,tz`,
  `SHOWCASE_ACCEPT_HIZ`, `SHOWCASE_ACCEPT_GPU_DRIVEN`), and one 3840x2160 frame with its time:

```sh
flock /tmp/regia-gpu-3.lock flock /tmp/ferrite-gpu-3.lock env REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1 VK_LOADER_DRIVERS_SELECT='*nvidia*' \
  cargo test -p showcase --test acceptance -- --ignored --nocapture --test-threads=1
```

`SHOWCASE_ACCEPT_LOOK=plain|film|film-fixed`, `SHOWCASE_ACCEPT_SIZE=WxH` and `SHOWCASE_KEEP_FRAMES=<dir>` change the look,
the size of the 120-frame job and keep the first and last frames of the first run.

## Limits

- The streamed scatter has no impostor far field in the showcase (trees end at their class's fade, about 4 km
  for the showcase trees) and no wind sway yet. `--entity-scatter` is the older static set of entities.
- A scene with no mesh at all would capture nothing (the engine renders a frame only with at least one mesh); the
  sample adds a 1 cm anchor box 50 m under the lowest ground.
- The sun clock is fixed for a run; weather changes over time, the hour does not.
- Contact AO uses the streamed scatter field only; the entity path (`--entity-scatter`) keeps the old look.
- The entry streak is drawn by the translucent VFX layer, so it appears only once the runtime VFX layer exists
  (after the first frame).
- There is no `Terrain` component, so the `terrain.*` agent methods that read one report no terrain.
- A headless app does not push the extracted sky into the renderer by itself (the windowed runtime does when
  it presents); the sample does it every frame in `Render`.

## Look checks (ADR-2486)

`tests/look_checks.rs` holds measured look checks that need no goldens:

- `mobile_low_lite_forest_draws_a_scene` (runs by default): the lowest tier, Mobile Low with the lite forward
  shader forced (`render.set_forward_lite { tier: "low" }`), still draws a lit sky over terrain and trees.
- `output_dither_off_is_bit_identical_and_on_breaks_the_sky_bands` (runs by default): with
  [output dither](output-dither.md) off a frame is bit-identical to the default frame, and with it on the sky's
  bands are shorter and its mean brightness is unchanged.
- `lite_tier_forest` (ignored): the forest view at Desktop High, Desktop Low, Deck, Mobile High (lite) and Mobile
  Low (full and lite shader), with frame statistics.
- `sky_at_4k` (ignored, long on lavapipe): the clear sky, the night sky and a cloudy sky at 1920x1080 and
  3840x2160 from the same camera, plus a view toward the sun. It prints the longest band down a sky column, the
  star count and peak, and the high-frequency energy of the clouds. `LOOK_CHECK_CALLS='[["render.set_output_dither",
  {"enabled": true}]]'` runs the same shots with the dither on; `LOOK_CHECK_TAG` names the run.

```sh
LOOK_CHECK_OUT=/tmp/look cargo test -p showcase --test look_checks -- --include-ignored --nocapture
```

### What each tier cuts in the forest

What a scene loses as the tier drops, read from the render profiles and seen in the forest view on lavapipe
at 1280x720. Desktop High is the reference look.

| Tier | Sky | Clouds | Shadows | Light and surface | Image |
|---|---|---|---|---|---|
| Desktop High | Physical sky (sky-view and aerial tables), stars at night | Volumetric | 4 cascades of 2048², paged; long tree shadows across the meadow | Probe GI, screen-space GI, SSR, reflection probes, GTAO, contact shadows, parallax | Native scale, temporal AA, bloom, auto exposure |
| Desktop Low (integrated GPUs) | Analytic gradient, no stars or moons | None | 2 cascades of 2048²; tree shadows only near the camera | Ambient GI only, half-res SSAO, contact shadows; no SSR, probes or parallax | 0.75 scale, FXAA |
| Deck | As Desktop Low | None | As Desktop Low | As Desktop Low | 0.7 scale with temporal upscale, 40 fps cap |
| Mobile High | Analytic gradient | None | 2 cascades of 2048²; tree shadows near the camera | Lite shader (`high`) on tile GPUs: ambient GI, no SSAO or contact shadows | 0.85 scale, SMAA |
| Mobile Low | Analytic gradient | None | 1 cascade of 1024²: with the lite shader (tile GPUs) it reaches 40 m; on any other adapter it spans the whole view and the trees cast **no visible shadow** unless `render.set_sun_shadow_range` caps it ([Sun shadow range](sun-shadow-range.md)) | Lite shader (`low`) on tile GPUs: 4 lights per cluster, no local shadows | 0.7 scale, no AA (stair-stepped tree edges), no bloom, **no auto exposure** |

Mobile Low has no auto exposure, so it holds a fixed exposure. A scene's exposure band (`look.min_ev100` and
`max_ev100`, its centre) sets it, and so do the camera's `exposure_ev` and `--exposure-ev`. A scene with none of
these used to print at the engine's default EV100 0, which turns a sunlit forest pure white. The showcase now
holds the plain look's sunlit EV100 14.5 there. A night scene on Mobile Low still needs its own band or camera EV.

Mobile Low's one shadow cascade covers the whole shadow range when the lite shader is off: on desktop and
integrated GPUs and on lavapipe its texels are metres wide and the forest draws no tree shadows. A cap of
80-150 m (`render.set_sun_shadow_range {"range_m": 150}`) brings them back near the camera; on phones the lite
path already stops the cascade at 40 m. The showcase leaves it unset, so its frames are unchanged.

## Walking the scene in a headset (`--xr`)

`--xr simulator|openxr|auto` adds the engine's XR plugins and walk mode (ADR-2457, [XR walk mode](xr-walk.md)):
the rig starts at the scene camera's ground point facing where it looks, the floor follows the heightfield under
the head, the head is the only (binaural) audio listener, and the walk starts with the XR render budget for a
90 Hz display. `--xr-preset comfortable|moderate|free` picks the comfort preset (default `moderate`). With
`--xr` the scatter mesh defaults to level 2 (a cheaper foliage tier; `--scatter-lod` still wins) and mesh LODs
switch at the budgeted eye size (1152 x 1267). The desktop camera, its path and its shake never move the eyes.
The other way round, while the walk runs the desktop camera follows the head and its field of view widens to hold
both eyes (`showcase::xr::camera_follows_head`): the streamed forest, mesh LODs and the terrain stream and cull
around the primary camera, so the eyes always see what the head faces. A `camera.set_path` has no effect then.

```sh
# No headset: the simulator, headless (its clock runs so the walk moves), agent on.
cargo run -p showcase -- samples/showcase/example_scene --xr simulator --project-dir /tmp/walk
# A window with the desktop mirror (left eye).
cargo run -p showcase -- samples/showcase/example_scene --window --xr simulator
```

`openxr` and `auto` need the OpenXR binding, which is not built yet (spec 58 § 3.4): XR stays off with
`last_error.code` `xr_no_loader` in `xr.get` and the scene runs on the desktop. Drive the walk with
`xr.walk_start`, `xr.walk_set`, `xr.drive { action: "input", name: "move", ... }` and read it in `xr.get.walk`.
The trees have no colliders, so a walk can pass through trunks.

## Interactive demo

`showcase --demo` runs the scene as an interactive demo (free-fly camera, sun and weather sliders, tier picker, frame-rate
overlay, screenshots, benchmark, credits) with its own `demo.*` agent methods: see [Showcase demo](showcase-demo.md).

## The forest shot and the dense-scene path

A non-plain Desktop High or Desktop Ultra run (the default forest shot, and `--render-profile DesktopUltra`)
asks for `RenderProfile::dense_scenes`. That profile turns the visibility buffer on and selects it while the
scene is dense (`VisibilitySelect::Dense`), with the single-pass resolve requested. Plain mode
(`--plain`), Mobile Low, Mobile High, Desktop Low and Deck stay on `RenderSettings::from_profile`, so their
`visibility_buffer` stays off.

Probe GI and the temporal upscaler stay the preset's. Desktop High keeps `gi_probes` and
`TemporalUpscaler::Taau`. The host does not set a lighting option, and it does not switch the upscaler.
If the engine turns the path off after startup, the host leaves that status; it does not write the switch
back on. `tests/dense_scenes.rs` checks the settings and a few headless updates of the example scene.
