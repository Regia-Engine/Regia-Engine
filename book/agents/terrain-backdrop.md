# Terrain far backdrop ring

A playable terrain ends at its border. Past the border, a wide view shows the sky or the
plane under the horizon. The backdrop ring fills that space with a low, visual-only
heightfield. The ring is drawn by the same terrain draw with the same material layers, so the
border shows no seam and no change of material (ADR-8459).

The ring is **visual only**. The collider, the navigation bake and every simulation query keep
the playable (inner) map. The ring has no collision, no navigation and no holes.

Method: `terrain.backdrop_status` (read only, no parameters, no error codes of its own).

## How a host builds it

```rust,wrap
# use regia_app::prelude::World;
# use regia_render::QualityTier;
# use regia_terrain::{BackdropRing, TerrainMap, build_visual_map};
# let mut world = World::new();
# let tier = QualityTier::High;
# let inner_map = TerrainMap::from_heights(129, 129, vec![0.0; 129 * 129], 128.0, 1.0, 0.0)?;
let ring = BackdropRing::default();
let build = build_visual_map(&inner_map, &ring, Some(tier))?;
let collider = inner_map.collision_heightfield(None); // the inner map, always
let drawn = build.visual_or(&inner_map); // the extended map, or the inner one
# let _ = (collider, drawn);
world.insert_resource(build.status); // what terrain.backdrop_status reports
```

`build_visual_map` copies the inner map sample for sample into the centre of a larger map. It
then fills a ring of `ring_texels` samples on every side, continued from the inner map's edge:

- the edge height eases to a plateau `plateau_drop_m` below the mean height of the border;
- the ring follows a box average of the border that widens with distance, so a pit or
  ridge cut by the border does not streak out into the ring;
- ridge noise (`ridge_wavelength_m`, `ridge_amplitude_m`) grows outward from zero at the seam;
- a far `landform` fades in: `none`, `terraces` (stepped, gullied cliffs rising to a plateau)
  or `chaos_blocks` (tilted polygonal blocks with grooves and pressure ridges).

The extended map keeps the inner map's height scale and offset and its grid origin (ADR-8179).
A rectangular inner map (`TerrainMap::with_world_depth`, ADR-8369) gets a rectangular result
with the same ring on every side. It is never padded out to a square.

**No seam.** The ring's first texel row on each side holds the inner edge sample exactly. The
drawn surface between the last inner sample and the first ring sample is therefore flat at the
edge height. The height difference across the seam is 0, even where the larger map's world
coordinates round differently. `cargo test -p regia_terrain --test suite backdrop_seam_gap_zero`
checks every border sample, every corner and the bilinear band between them.

## Settings (`BackdropRing`, a resource)

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Master switch. |
| `ring_width` | `inner_fraction(0.5)` | Ring samples per side: a fraction of the inner map's longer side (floored), or `texels(n)`. |
| `max_side` | `8192` | Largest side of the extended map. The ring shrinks to fit. |
| `min_ring_texels` | `64` | A ring narrower than this is not built (`no_room`). |
| `seam_ease_m` | `20.0` | Metres over which the ring eases from the exact edge into the border average. |
| `off_tiers` | `[MobileLow]` | Quality tiers that never build the ring. MobileLow is Regia's lowest ("very low") row. |
| `profile` | see below | The ring's shape. |

`RingProfile` defaults: `ridge_wavelength_m` 420, `ridge_amplitude_m` 5, `plateau_drop_m` −20,
`landform` `none`. A project keeps one profile per map as data. A per-map file may override
only some keys: `RingProfile::overlaid_ron("(ridge_amplitude_m: 30.0, landform: terraces)")`,
or `overlaid_json` for a JSON file (unknown keys such as `_comment` are ignored). A
non-positive wavelength, an unknown landform or a non-number is refused.

Memory: an extended 8192 × 8192 map holds 256 MiB of `f32` heights on the CPU, plus the GPU
heightfield and its mips. This is why the lowest tier skips the ring.

## terrain.backdrop_status

```json
{"method":"terrain.backdrop_status","params":{}}
```

```json
{
  "state": "built", "enabled": true, "configured": true, "tier": "High",
  "inner_size": [4096, 4096], "inner_world": [1280.0, 1280.0],
  "visual_size": [8192, 8192], "visual_world": [2560.31, 2560.31],
  "ring_texels": 2048,
  "collider": "inner: the collider, nav and simulation keep the inner map",
  "settings": { "enabled": true, "ring_width": { "inner_fraction": 0.5 }, "max_side": 8192,
                "min_ring_texels": 64, "seam_ease_m": 20.0, "off_tiers": ["MobileLow"],
                "profile": { "ridge_wavelength_m": 420.0, "ridge_amplitude_m": 5.0,
                             "plateau_drop_m": -20.0, "landform": "none" } }
}
```

`state` is one of:

- `built`: the draw takes the extended map;
- `disabled`: `enabled` is false;
- `tier_off`: the tier is in `off_tiers`;
- `no_room`: the map is within `min_ring_texels` of `max_side`;
- `not_built`: the host has settings but has not built yet.

`enabled` is true only for `built`. Without a `BackdropStatus` or `BackdropRing` resource,
`configured` is false and the default settings are reported. In every state except `built`,
`visual_size` equals `inner_size` and `ring_texels` is 0.

## Verify

- `cargo test -p regia_terrain --test suite backdrop`: seam gap zero, the collider untouched,
  the lowest tier skipped, a rectangle extended without padding.
- `cargo test -p regia_terrain --lib backdrop`: the ported cases (centre copied exactly, ring
  continuous, a border pit does not streak, the spec scales and caps at 8192, the profile
  overlay).
- `cargo test -p regia_agent --lib backdrop_status_reports_the_ring`: the method through the
  dispatcher.

A rendered plate of the ring is not blessed yet (see the ADR); judge the look with
`render.screenshot` on a wide view.
