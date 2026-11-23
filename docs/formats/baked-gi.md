# `.fgiv` — baked diffuse GI volume (format v4)

Writer: `regia bake-gi <scene>` (`regia_pt::probe::bake_gi_volume`). Reader:
`regia_asset::baked_gi::BakedGiVolume::{from_bytes, read}`. Plan:
`docs/research/strategic-review-20261012.md` § 3 (baked lighting, phase 1), ADR-206.

A volume stores, per probe, the **incident radiance** as L1 spherical harmonics for four separable
light components, octahedral **distance moments** for visibility, and a **meta** record
(relocation offset, validity flags). Each component is linear in its light, so the host rescales it
at runtime by its current light over the baked one (`BakeLights`).

| Component | Holds | Rescale by |
|---|---|---|
| `sun_bounce` (0) | sun light after ≥ 1 bounce (the sun seen directly is live) | sun lux / `sun_illuminance` |
| `sky` (1) | sky seen directly (sky visibility) and bounced | sky scale / `sky_scale` |
| `fill` (2) | sky bodies (planet-shine), shadowed, direct and bounced | fill lux / `fill_lux` |
| `emissive` (3) | static emissive surfaces, direct and bounced (v3: the header names them) | `emissive_scale` |

Live, never baked: the sun disc, point/spot/sphere lights, stars.

## Layout

One probe index space: the terrain layer first, then the detail bricks in order.

- **Terrain layer** (2.5D): columns at `origin + (i, j)·spacing` (XZ), each standing on
  `ground[j·nx + i]` (metres, stored), with one probe per `levels[l]` height above the ground
  (≤ 8, ascending; default 1, 2, 4, 8, 16 m). Index `(j·nx + i)·levels + l`. Runtime lookup:
  bilinear over the four columns, linear between the two levels that bracket the height above the
  interpolated ground.
- **Detail bricks**: regular grids (`origin`, `spacing`, `counts`, `flags`), index
  `first_probe + (z·ny + y)·nx + x`, placed around tagged static meshes (phase 4). A brick
  overrides the terrain layer inside its box.
- **Brick flags** (v2, `BrickFlags`): bit 0 `INTERIOR`, an enclosed interior. Inside it the
  terrain layer contributes nothing and the brick does not blend out over its last cell toward
  the outdoor probes. Bit 1 is reserved. A reader refuses a file with bit 1 or any higher bit set.
  The bake sets `INTERIOR` on the bricks whose centre lies inside an interior box
  (`VolumeLayout::flag_interior_bricks`).

Probe world position = nominal position + meta offset.

## File

```
"FGIV" | u32 version (5) | u32 header_len | postcard VolumeHeader | pad to 16
section 0  meta      probes × 8 B   f16 offset x, y, z; u16 flags
section 1  sh        probes × 4 components × 16 B
section 2  moments   probes × res² × 4 B
section 3  ground    nx × nz × f32 (terrain layer only)
```

Every section starts on a 16-byte boundary (map-friendly); offsets and lengths are in the header,
with a blake3 hash over the four sections in order. All integers little-endian.

**Versions.** v5 (current, ADR-3237) lets a split part hold a **crop** of a brick that crosses a cut.
Each `bricks` record of the split section gains `source_counts` (the unsplit brick's probe counts),
`crop_offset` (the unsplit cell of the part brick's cell 0, 0, 0; 0 in y) and `owned` (the part brick's
cells this part owns, local `[x0, z0, x1, z1]`, half-open, every y); the part brick itself is the crop
(its origin moved by whole cells, its counts the crop's). Each unsplit cell has exactly one owner part:
the one whose columns hold it. A bake crops only when no split that keeps every brick whole fits, so a
layout that split before splits into the same parts (no crop, offset 0, the owner part owns every cell
of its bricks). The part keeps the brick's cells over its rectangle plus a margin past each cut side:
at least 2 cells, and more where a brick of the same class (interior or not) overlaps it, so the brick
pick inside the part's rectangle is the unsplit one (ADR-3237). A cropped split cuts on a fine grid
(the finest brick cell), so its `band` is one fine cell wide and its `rect` edges need not lie on
terrain columns; the parts still store the terrain columns their lookups read. Only the split
section changed: an unsplit volume's header is the same bytes in v4 and v5 (the version word
differs). v4 parts read with no brick cropped, and `regia upgrade-project --write` rewrites v1 to v4
files at v5 (`migrate_baked_gi_file`, sections byte for byte); a reader refuses a crop record whose
offset, counts or owned cells do not fit the brick or the unsplit layout.

v4 (ADR-2618) adds `split: Option<VolumeSplit>` at the end of the header:
`None` for a whole volume, `Some` in each part file of a **split volume**. A layout above the portable
128 MiB binding (`PORTABLE_VOLUME_BYTES`) bakes as 2 to 4 parts (`MAX_SPLIT_PARTS`). Part 0 is at the
named path and part `k` at `<stem>.part<k>.fgiv` beside it. Each part is a complete volume (its own
terrain columns, bricks and sections), and its records are byte copies of the unsplit bake's. The
section holds `group` (u64, the same in every part of one bake), `index`, `count`, `tile` and `tiles`
(`index = tile.z · tiles.x + tile.x`), `rect` (the blend rectangle, world min x, min z, max x, max z)
and `band` per edge (`-x, -z, +x, +z`, metres; 0 on the map's own border, above 0 toward a neighbor). It also holds `neighbors`
per edge (`u32::MAX` = none), `source_probes`, `source_columns`, `terrain_offset` (the unsplit column of
the part's column 0, 0), `owned_columns` (local `[i0, j0, i1, j1]`, half-open) and `bricks` (per brick:
`source`, `source_first`, `owner`; v5 adds the crop fields above). Every unsplit probe has exactly one owner. Neighbors share a band
one terrain cell wide, and each part also holds one guard column past it. A reader refuses a section
whose counts, tiles, indices, owners, bands or column ranges do not fit the part's layout. Versions 1
to 3 read as one unsplit volume, and `regia upgrade-project --write` rewrites them at v4
(`migrate_baked_gi_file`, sections byte for byte).

v3 (ADR-1386) adds `emitters: Option<Vec<BakedEmitter>>` at the end of the
header: the emitters the bake traced into the `emissive` component, each the world bounds `min:
[f32; 3]`, `max: [f32; 3]` of one placed emissive mesh (the object-space bounds of the vertices its
triangles use, under its object-to-world transform: the box around the eight transformed corners,
`BakedEmitter::from_object_bounds`). `Some` (possibly empty) when the bake recorded them; a reader
refuses non-finite bounds or `min > max`. A renderer that also draws emissive meshes as area lights
matches each drawn emitter's bounds against the list (every corner coordinate within 2 cm plus 2 %
of the record's diagonal, `BakedEmitter::matches`) and leaves a matched emitter's light to the
volume where it covers. The four sections are unchanged. v2 adds `flags: u32` after `first_probe`
in every `DetailBrick` of the header; nothing else changes, and the four sections are the same
bytes. A v2 file reads with `emitters` `None` (not recorded): a renderer then keeps every area
light, so its emitters are counted twice on the tiers that draw them, until the map is rebaked.
`to_bytes_v2` writes v2 and refuses a volume with an emitter list. v1 files (no brick flags)
still read, with every brick's flags 0, and look up exactly as before (bit for bit on the CPU
mirror). `BakedGiVolume::to_bytes` writes v2; `to_bytes_v1` writes v1 for an older reader and
refuses a volume with brick flags or an emitter list. `.fgiv` is a baked output, but since v4 it has an
`upgrade-project` row (`baked-gi`) and a `check-formats` row: `regia upgrade-project --write` rewrites
v1 to v3 files at v4 (ADR-2618), and rebaking works as before. Rewriting a v1 or v2 file as v3 keeps `emitters` `None`; only a bake can
name its emitters.

**SH record** (16 B per component): bytes 0–3 RGB9E5 (`EXT_texture_shared_exponent`) of
`L00 · exposure[c]` per channel; bytes 4–12 snorm8 of `L1m / (√3 · L00)` for
`[r: x y z, g: x y z, b: x y z]` (non-negative radiance bounds the ratio to ±1); 3 bytes padding.
Basis: `Y00 = 0.282095`, `Y1 = 0.488603 · (x, y, z)`. Irradiance on a surface facing `n`:
`E = π·Y00·L00 + (2π/3)·Y1·(L1·n)`, clamped at 0 (`baked_gi::irradiance`). `exposure[c]` is a
power of two that maps the component's peak L00 to ≤ 32768.

**Moments** (4 B per texel): unorm16 of `mean / max_distance` and `mean² / max_distance²`;
texel `(x, y)` of a `res × res` map looks along `oct_decode((x + 0.5, y + 0.5) / res)` (octahedral,
fold on −Z, Cigolle et al. 2014). Visibility of a point at distance `d` along `ω`: Chebyshev on the
bilinear moments toward `ω` (`baked_gi::chebyshev`). The runtime lookup by default takes the bound per
bilinear tap and filters the bounds, then applies a light-bleeding reduction (ADR-1105); the file is the same.

**Flags**: `VALID` 1, `RELOCATED` 2, `UNDER_TERRAIN` 4 (invalid), `INSIDE_GEOMETRY` 8 (invalid:
more than 25 % back-face hits after relocation). Bits 4–7, 8–11 and 12–15 (`WALL_X`, `WALL_Y`,
`WALL_Z`, ADR-366) hold a 4-bit wall code toward the next probe along +X, +Y (the next level in the
terrain layer) and +Z: 0 when the segment between the two probes crosses no geometry, else k = 1..15
puts the wall at (k − 0.5)/15 of the segment from this probe (`ProbeFlags::wall_at`). Volumes baked
before ADR-366 have no codes; a reader that predates them ignores them. The format version did not
change.

## Runtime (phase 2)

`FrameRenderer::set_baked_gi` uploads the meta, SH and moment sections unchanged into one storage
buffer (then the ground heights and a 12-word brick table: origin xyz + `first_probe`, spacing xyz +
`flags`, counts xyz + 0; the flags use the word that was padding in v1, so the table did not grow).
`baked_gi.wgsl` `static_gi(p, n, v)` reads it in place: meta word 0 = f16 x | f16 y,
word 1 = f16 z | flags; SH words = RGB9E5 L0, then the nine snorm8 L1 ratios; moments = unorm16
mean | mean². The lookup and the baked-map policy are in ADR-207 (phase 2) and
`book/agents/baked-gi.md`.

**Near field** (ADR-281, ADR-282). The lookup picks an `INTERIOR` brick over any other brick that
holds the point. It tests the point pushed half the brick's smallest cell along the receiver's
normal, so an outer wall reads the outdoor light and an inner wall reads the interior's. The test
allows an eighth of a cell outside the probe box (ADR-1364), so a surface on the box's face stays
inside. An interior brick's lattice should lie on the room's air box (the walls' inner faces). Inside,
the coverage is 1 and the terrain layer is skipped. With no valid corner the interior reads black,
so no outdoor light leaks in. Capsule occluders for dynamic units (`FrameRenderer::set_gi_occluders`,
at most 64 a frame) darken the baked light in the screen pass only. They are not part of the file.

**Leak control** (ADR-366). Where the eight corner probes of a lookup cell carry wall codes, the
lookup keeps only the corners on the receiver's side of the wall: the corners the cell's unblocked
edges join to the corner the receiver lies toward (per axis, past the walls' interpolated position
or not). A doorway leaves edges unblocked and joins the two sides. Without codes the lookup is the
v2 lookup above, unchanged.

**Quality pass** (ADR-861, runtime only; the file is unchanged). The lookup has three host switches
(`BakedGiSettings`), all on by default:
- `deringing`: a strongly directional L1 is evaluated as a non-negative lobe with the same mean and
  first moment.
- `leak_guard`: a wall code on an edge to a rejected probe neither joins nor moves the wall.
- `sun_relight`: the sun bounce of probes lit from below follows the sun's elevation.
Volumes baked before read the same with the switches off; with them on, they read as a new bake would.

**Several volumes** (ADR-1097). Up to 8 volumes (the installed one and `FrameRenderer::set_baked_gi_layers`)
share the one storage buffer, each one's sections after the previous volume's. The screen pass's uniform
holds a `BakedGiUniform` per volume, its word offsets pointing at that volume's sections. The file is
unchanged; how the volumes blend across their seams is in `book/agents/baked-gi-seams.md`.

## Bake description (`.ron`, provisional)

`regia bake-gi map.bake.ron` reads `regia_pt::bake_desc::GiBakeDescription`. Not frozen yet: it
gains `format_version` and a `check-formats` entry when DO27's cook adopts it (phase 3).

```ron
(
    // Raw little-endian u16, row 0 at −Z, centred on the origin (TerrainMap convention).
    terrain: Some((heights: "bore12.r16", width: 1025, depth: 1025, world_width: 1280.0,
                   height_scale: 300.0, height_offset: -40.0, albedo: (0.32, 0.30, 0.28))),
    sun: Some((toward: (0.3, 0.5, 0.2), illuminance: 50000.0, angular_radius: 0.00465)),
    sky: None, // airless
    sky_bodies: [(direction: (-0.4, 0.6, 0.7), angular_radius: 0.12, radiance: (900.0, 820.0, 700.0),
                  light_scale: 1.0)],
    layout: (terrain_spacing: Some((8.0, 8.0)), levels: [1.0, 2.0, 4.0, 8.0, 16.0],
             bricks: [(min: (10.0, 0.0, 10.0), max: (40.0, 12.0, 30.0), spacing: 1.0)],
             moment_resolution: 4, max_distance: None),
    bake: (rays_per_probe: 256, samples: 16, max_bounces: 8, seed: 1, probes_per_batch: 4096,
           relocation_iterations: 2, filter: true),
)
```

Built-ins: `cornell`, `furnace`, `rolling-hills`. Command-line overrides: `--rays`, `--samples`,
`--bounces`, `--seed`, `--probes-per-batch`, `--no-filter`, `--out`. The command prints a JSON
summary (output path, bytes, probe counts, valid / relocated / invalid, rays, seconds, device).
