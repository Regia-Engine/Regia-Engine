# Tunnel acceptance scene

One synthetic scene that exercises the tunnel features together, so "a tunnel is one tested set" can be
shown with saved frames and numbers. It runs on lavapipe with no DO27 content.

```
cargo test -p regia_terrain --test tunnel_scene                      # the asserted properties
REGIA_DUMP_DIR=/tmp/out cargo test -p regia_terrain --test tunnel_scene -- --nocapture
REGIA_TUNNEL_FPS=30 REGIA_DUMP_DIR=/tmp/out cargo test ...         # full-rate frames
REGIA_TUNNEL_ZONE=8.5,10.5,25,8,8,15 REGIA_TUNNEL_TURN_Z=60 REGIA_SHAFT_RULE=legacy cargo test ... # by hand: a zone tuning, the zone drive's turn, the other fog rule
```

`REGIA_DUMP_DIR` writes `tunnel_scene/{old,on}/frame_NNN.png`, `metrics.csv` and `contact_sheet.png`
(top row: old behavior, interior lit as open sky; bottom row: tunnel set on; `contact_sheet.txt` lists the columns).

## The scene

World axes: the tunnel runs along +Z. The ground is flat (rolling hills beyond 150 m).

| Piece | What |
|---|---|
| Terrain hole | `HolesMask` rectangle, 10 m × 20 m, z −20…0 (`TerrainSharedDraw::set_holes`); the collider cut is checked with `HolesMask::cut_heights` |
| Ramp and tube | Floor descends 6 m over the cut, then a covered tube 10 m wide, 5.5 m high, z 0…80, roof just under the ground |
| Lip | Four green strips flush with the ground (1 cm under it) on the inside of the cut, reaching 0.25 m under the ground past the edge; the trench walls, headwall and the ramp's top end stop under the lip |
| `InteriorVolume` | The covered tube (interior shade: no sky IBL inside, 40 lux floor) |
| Fog | A `FogVolume` shaft over the open cut and dust over the first 40 m of the tube |
| Headlight | A spot light on the camera plus a matching `FogLight` for the dust |
| Sky, exposure | Procedural Earth sky at noon (image-based lighting), auto exposure |
| Camera | In from z −50 to +40, eight-frame turn, back out, at 20 m/s (sampled at 10 fps; 2 m a frame) |

`Variant::Old` is today's default (no interior volume, no fog; `FrameQuality::interior_ambient` off).
`Variant::On` is the tunnel set as it exists today (interior volume, fog shaft, headlight dust) with every slot
filled (below).

## Measurements (per frame)

- **Mean luminance** (BT.709, 0–255): no black frame (mean > 3, under 90 % black pixels) and no frame-to-frame step above `FLASH_BOUND`.
- **Terrain inside the cut**: each pixel whose ray crosses the ground plane inside the cut (shrunk 0.3 m, eroded 2 px) must not be ground-coloured. A control renders the uncut ground and must be flagged.
- **Crack detector**: along the cut edge every 0.25 m, the pixels within 1.5 px of the seam must be ground, lip or a blend (the lip read on its centre line, the ground 0.5 m outside the cut, any ground colour along the same edge). A control with a lip that stops 0.5 m short must be flagged.

Each path starts with the exposure metered on its first view (`TunnelScene::warm_up`): a fresh renderer's first histogram is adopted outright two frames in, a start-up step the drive does not measure.

## Slots: the tunnel features

`scene::Slots` holds one `Slot { name, owner_lane, fill: Option<Box<dyn SlotFill>> }` per tunnel feature.
`Slots::none()` leaves every slot empty (the old path and the detector controls render with it, so their frames
do not move when a feature changes); `Slots::today()` fills all six from `fills.rs`, and the on path renders with
them. A `SlotFill` has two hooks, both no-ops by default: `apply` receives the `TunnelGeometry` (interior box,
mouth, camera, frame step) and the `TunnelParts` (outdoor params, post settings, frame quality, the lab scene, the
exposure-zone target, the baked GI volume) every frame before it renders; `place` spawns the feature's entities in
the scene's ECS world (`scene::tunnel_world`: the cut terrain plus every slot's entities).

| Slot | Feature (module) | Filler | Test and what it measures |
|---|---|---|---|
| `exposure_zone` | `ExposureZone` (`regia_render::exposure_zones`) | box from 15 m into the tube to the end cap (plus the cook's 0.25 m pad), the cook's band EV 8.5–10.5 and 25 m blend (spec 121 § 5.6, `regia_world_geom::sections::ZONE_EXPOSURE`), 8 EV/s; weighed at the eye and combined as `regia_framework::sync_exposure_zones` does | `exposure_zone_eases_the_mouth` (own drive, turning 60 m in, where the tube meters under the band: ADR-8629): at the turnaround the zone holds the eye in its band and the plain meter is under it; against the same drive without the zone, the worst step entering the tube is lower (14.9 vs 20.2) and the drive's worst no higher (14.9 vs 20.2). `exposure_zone_removes_the_entry_flash`: on the same drive no step above `FLASH_BOUND_WITH_ZONE` (25; 14.9 measured; ignored before ADR-8629) |
| `reverb_zone` | `AudioReverbZone::tube` (`regia_audio`) | box over the tube | `reverb_zone_covers_the_tube`: `placed_zones` finds one tube-preset zone; every tube corner inside; the drive is wet in the tube and dry past the fade |
| `probe_volume` | baked GI (`regia_render::baked_gi`, `regia_pt` CPU bake) | ground-following layer over the tube (2.25 × 2.5 m columns, levels 0.5–4.5 m), 256 rays a probe, baked once per test binary in memory; MSAA off (the volume lights single-sample frames) | `probe_volume_lifts_the_tube_interior`: with no headlight, no fog and fixed exposure, the floor 6–16 m in is brighter lit as a baked tunnel (interior shade off) than under today's interior shade; the CPU lookup covers and carries bounce at every depth. Inside an interior volume the shade ignores a baked volume (ADR-3257) |
| `interior_fog` | `FogVolume::interior` (`regia_render::volumetrics`) | fog bounded by the `InteriorVolume`, 0.008 /m | `interior_fog_stays_in_the_tube`: readback with and without the fog; outside the interior's screen box (+24 px of froxel blur) no pixel moves more than 2 levels; the sunlit interior air shows |
| `light_shafts` | the froxel fog's sun shadow (`regia_render::fog`) | the shaft's sun-shadow rule | `light_shafts_fall_through_the_cut`: black surfaces, no headlight; per pixel a ray-cast oracle splits the air into sunlit and shadowed; the cut's sunlit air glows, the sunlit wedge under the portal glows, air the roof shadows glows by at most 15 % of its depth (the `calibrated` rule: 9 %; `legacy`: 35 %, fails) |
| `world_pick` | `world_pick` layers (`regia_pick`) | the tube meshes on pick layer 1 | `a_pick_ray_into_the_mouth_hits_the_tube`: rays through an approach frame that meet the ground inside the cut hit the tube (layer 1, at the triangle oracle's distance) and fall through on layer 0; rays outside hit the terrain |
