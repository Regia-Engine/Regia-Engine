# Tunnel acceptance scene

A tunnel is several features that have to work as one set: a hole in the terrain, a tube and lip under it,
an interior volume, local fog, a headlight with dust, the sky and auto exposure. The tunnel acceptance
scene builds one synthetic tunnel from those pieces and drives a camera in and out of its mouth at 20 m/s,
on lavapipe, with no game content.

It is the test target `tunnel_scene` of `regia_terrain` (`crates/regia_terrain/tests/tunnel_scene/`; the
README there lists the scene and its slots).

```text
cargo test -p regia_terrain --test tunnel_scene
REGIA_DUMP_DIR=/tmp/out cargo test -p regia_terrain --test tunnel_scene -- --nocapture
```

With `REGIA_DUMP_DIR` set the run writes every frame of the path for the old behavior (the interior lit as
open sky, today's default) and for the tunnel set on, a `metrics.csv`, and `contact_sheet.png` with the old
behavior on the top row and the tunnel set on the bottom row.

## What each frame is checked for

- Mean luminance: no black frame, no frame-to-frame jump above a bound (a flash when the exposure catches up).
- No terrain pixel inside the hole's footprint (checked against an uncut control that must be flagged).
- No crack along the lip at the cut edge (checked against a lip that stops short, which must be flagged).

## The tunnel features (slots)

Exposure zones, reverb zones, a baked probe volume, interior fog, light shafts and world pick layers each
have a named slot in the scene, filled with the feature as it ships (ADR-3257; `fills.rs`). The old path
renders with every slot empty, the tunnel set with all six. One test per slot:

- `exposure_zone_eases_the_mouth`: an exposure zone over the tube (the cook's band for a tunnel section, EV
  8.5 to 10.5 with a 25 m blend) lowers the worst frame-to-frame step entering the tube against the same drive
  without it (14.9 against 20.2 levels), and the drive's worst is no higher (14.9 against 20.2). The zone tests
  drive to 60 m in, where the baked tube meters under the band; the test asserts that the zone holds the eye in
  its band there while the plain meter is under it (ADR-8629). `exposure_zone_removes_the_entry_flash` holds the
  same drive to 25 levels (14.9).
- `reverb_zone_covers_the_tube`: one tube-preset reverb zone covers every corner of the tube; the drive is
  wet inside and dry past the zone's fade.
- `probe_volume_lifts_the_tube_interior`: a baked GI volume of the tube (CPU bake in the test, well under a
  second) lights the floor 6 to 16 m in brighter than the interior shade's flat floor. The test lights the
  tube as a baked tunnel without an interior volume (it predates ADR-7975; since then the interior shade
  reads a covering baked volume too, `interior_baked_gi`).
- `interior_fog_stays_in_the_tube`: fog bounded by the interior volume (`FogVolume::interior`) leaves every
  pixel outside the interior's screen box unchanged and shows in the sunlit air under the portal.
- `light_shafts_fall_through_the_cut`: with black surfaces and the headlight off, a ray-cast oracle splits
  each pixel's air into sunlit and roof-shadowed parts; the cut and the wedge under the portal glow, the
  shadowed air does not (the `calibrated` fog sun-shadow rule; `legacy` fails it).
- `a_pick_ray_into_the_mouth_hits_the_tube`: `world_pick` rays through an approach frame hit the tube's
  meshes on their pick layer inside the cut, fall through the hole on the terrain layer, and hit the
  terrain outside it.

With `REGIA_DUMP_DIR` the slot tests also write their plates (`probe_volume_*.png`, `interior_fog_*.png`,
`shaft_*.png`).

This page is for the engine's own tests; it adds no agent-protocol method. To look at a tunnel in a running
game use `terrain.holes` and the screenshots described in [Tunnels](tunnels.md).
