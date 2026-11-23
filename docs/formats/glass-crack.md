# .glasscrack.ron — glass crack set v1

Example: `docs/formats/glasscrack.example.ron`. JSON schema: `docs/formats/glasscrack.schema.json`.
Runtime: `GlassDamage` and the stamping (`regia_render::glass`), chunk bursts and breaches
(`regia_vfx::glass`), cooker `regia_asset_pipeline::cook_glasscrack`, agent methods
`render.glass_damage` / `render.glass_view`, guide page `book/agents/glass.md`. Built-in sets
`laminate` and `pressure_pane` are procedural; a cooked set with the same name replaces one.

## Overview

A crack set is the authored look of damaged glass for one **glass type**. It is a manifest
(`<name>.glasscrack.ron`) plus a folder of stamp textures. At runtime every damaged glass instance owns
a **crack mask page** (an RGBA8 mask + an RG8 normal page). Each impact **stamps** one variant from the
set into that page at the hit point, so cracks move with the vehicle or building. The glass shader
reads the page; nothing is projected onto the scene.

Two glass types, chosen by `glass_type` (DO27 `spec/glass.md` "Glass types"):

| `glass_type`    | Used for                                   | D1                         | D2                                                 | D3 |
|-----------------|--------------------------------------------|----------------------------|----------------------------------------------------|----|
| `laminate`      | unit canopies and visors (polymer laminate)| bullseye impacts, spall chips | spreading spiderweb, crazing, milky delamination halo near hits | heavy crazing; small punched-through holes at the worst hits only. **Stays in the frame:** no falling sheets, no large holes, no shard showers |
| `pressure_pane` | building windows (thick multi-pane)        | outer-pane star cracks     | outer pane spiderwebbed; the inner pane stays clear behind it, with parallax | thick panes fail; chunky angular fragments through regia_vfx (not glitter); optional decompression breach |

Damage levels: **D0** intact (no page is allocated), **D1**, **D2**, **D3**. The game sets the level or
lets impacts escalate it (`escalation`). Each impact stamps from the set of the instance's level at the
time of the hit; earlier stamps stay.

## Mask channels (stamp `mask` texture and the runtime page)

All channels are **linear** 8-bit (not sRGB). 0 = intact glass.

| Ch | Meaning | Shading |
|----|---------|---------|
| R  | crack lines (main fractures, bullseye rings, spall chip edges) | transmission × `1 - R·(1 - crack_transmission)`; frosted white scatter `crack_scatter` added with weight R; roughness → `crack_roughness` |
| G  | crazing / clouding density (fine micro-cracks; laminate: delamination haze) | milky haze `crazing_haze·G` (lit diffuse, not refracted); roughness → `crazing_roughness` |
| B  | missing material (holes) | alpha-tested at `hole_cutoff` with a jagged, noise-broken edge: see-through and shadow-free. **Must be 0 in D1/D2 stamps.** |
| A  | crack normal-bend strength | scales the stamp's crack normal for lighting and for the refraction offset (`normal_bend`) |

Pixels outside the stamp's footprint must be `(0, 0, 0, 0)`. Stamps combine into the page with a
per-channel **max** (deterministic and order-independent), so overlapping hits never darken or erase each
other.

## Crack normal texture (stamp `normal`)

Tangent-space normal map, glTF/OpenGL convention (+X right = +U, +Y up = −V of the image, i.e. green up),
linear RGB 8-bit, same size as its mask. Z is reconstructed. Flat (128,128,255) outside the cracks.
Stamps combine into the runtime RG8 normal page by keeping the texel with the larger A. Optional: a stamp
without `normal` derives one from the gradient of R × A at cook time.

## Stamp textures

| | Rule |
|---|---|
| Source files | PNG, 8 bits per channel. Mask RGBA, normal RGB. Paths are relative to the manifest. |
| Size | Square, power of two, 128–1024. Recommended: 256 for D1, 512 for D2 and D3. Mask and normal of one stamp must match. |
| Footprint | `size_m` (width, height in metres) is the size of the whole texture on the glass. `pivot` (u, v in 0..1, origin top-left) is the impact point inside the texture. |
| Variants | 2–8 per level recommended (≥ 1 required). `weight` biases the random pick. The pick and a rotation (if `random_rotation`) come from a hash of the impact, so a save that keeps the impact list re-stamps identically. |
| Cooked form | The cook validates the stamps and stores them raw (RGBA8 + RG8) in the pak (zstd-compressed with the pak). They are **not** BC/ASTC compressed: stamping runs on the CPU into a dynamic per-instance page, which cannot be block-compressed per hit without a GPU encoder. Typical set: 4 variants × 3 levels at 256/512/512 ≈ 7 MiB raw, ≈ 1–2 MiB on disk. |
| Runtime page | `page_size`² RGBA8Unorm + RG8Unorm per damaged instance (512 → 1.5 MiB), the same on desktop and Android. Pages come from a fixed pool (`GlassDamageSettings::max_pages`, desktop 64, Android 16). When the pool is full the new page is half size; never silently dropped. |

## Where the page lies on the glass (`damage_space`)

- `planar` (default): the page is a planar projection in the instance's mesh-local space, along the
  axis of the smallest bounding-box extent (a window's normal, a canopy's up/out axis), covering the
  bounding box in the other two axes. Works with any UV layout (DO27 glass has one overlapping UV set).
  Texel size = bbox extent / `page_size`. Curved glass stretches toward grazing angles.
- `uv0`: the page maps the mesh's first UV set. Use only when that set is non-overlapping and
  roughly uniform in texel density; stamp size then uses the local UV-to-metre scale at the hit
  triangle (closest triangle to the hit).
- `uv1`: reserved. The runtime maps it like `planar` today (standard materials do not stream UV1).

## Manifest schema (`format_version: 1`)

RON with `deny_unknown_fields`. Optional fields may be omitted (`Option` fields default to `None`).

```
GlassCrackSet {
    format_version: u32,                 // must be 1
    name: String,                        // stable id (agent `set`, GlassDamage.set)
    glass_type: GlassType,               // laminate | pressure_pane
    page_size: u32 = 512,                // runtime page side: 256 | 512 | 1024
    damage_space: DamageSpace = planar,  // planar | uv0 | uv1
    shading: CrackShading,
    escalation: Escalation,
    d1: [CrackStamp],                    // >= 1 each
    d2: [CrackStamp],
    d3: [CrackStamp],
    laminate: Option<LaminateRules>,         // required iff glass_type == laminate
    pressure_pane: Option<PressurePaneRules>, // required iff glass_type == pressure_pane
}

CrackStamp {
    name: String,                        // unique within the set
    mask: String,                        // RGBA PNG (channels above)
    normal: Option<String>,              // RGB tangent-space PNG
    size_m: (f32, f32),                  // footprint in metres, each > 0
    pivot: (f32, f32) = (0.5, 0.5),      // impact point in the texture, 0..1
    weight: f32 = 1.0,                   // > 0
    random_rotation: bool = true,        // spin around the pivot per impact
}

CrackShading {
    crack_scatter: String = "#e6edf2",   // sRGB hex: frosted white along R
    crack_transmission: f32 = 0.1,       // transmission factor at R = 1 (0..1)
    crack_roughness: f32 = 0.7,
    crazing_haze: f32 = 0.55,            // haze opacity at G = 1 (0..1)
    crazing_roughness: f32 = 0.45,
    normal_bend: f32 = 1.0,              // crack-normal strength (lighting + refraction), 0..4
    hole_cutoff: f32 = 0.5,              // alpha-test threshold on B (0..1)
    edge_jitter: f32 = 0.35,             // jaggedness of hole edges (0 = clean cut)
}

Escalation {                             // cumulative impact energy to reach each level; 0 = never automatic
    d1: f32 = 1.0,
    d2: f32 = 4.0,
    d3: f32 = 10.0,                      // each > the previous, or 0
}

LaminateRules {
    max_hole_radius_m: f32 = 0.035,      // B never exceeds this radius around an impact (cook-validated, runtime-clamped)
    min_hole_energy: f32 = 3.0,          // only D3 impacts with energy >= this punch a hole ("worst hits only")
    halo_radius_m: f32 = 0.08,           // milky delamination halo (G) added around D2/D3 impacts
}

PressurePaneRules {
    pane_gap_m: f32 = 0.012,             // outer-to-inner distance for the parallax inner pane (> 0)
    inner_tint: String = "#f2f6f8",      // inner pane transmission tint (sRGB hex)
    chunks: ChunkBurst,                  // D3 fragment burst
    breach: bool = true,                 // D3 emits a GlassBreach (decompression) event
}

ChunkBurst {
    preset: String = "glass_chunks",     // regia_vfx preset
    count: (u32, u32) = (8, 16),         // min..=max fragments per D3 failure
    size_m: (f32, f32) = (0.06, 0.25),   // fragment edge length range (chunky, not glitter: min >= 0.03)
}
```

### Validation (cook and loader; all hard errors)

- `format_version == 1`; `name` non-empty; stamp names unique.
- `page_size ∈ {256, 512, 1024}`; textures square, power of two, 128–1024, mask/normal sizes equal.
- `laminate` present iff `glass_type: laminate`; `pressure_pane` present iff `glass_type: pressure_pane`.
- D1 and D2 masks have B = 0 everywhere (no holes before D3).
- laminate: every D3 mask's B texels lie within `max_hole_radius_m` of the pivot.
- pressure_pane: `chunks.size_m.0 >= 0.03` and `count.0 >= 1`.
- `escalation` thresholds strictly increase (ignoring zeros).

## Runtime save form

The stamped page is not saved. `GlassDamage` saves its level, accumulated energy and the impact list
(`[(point_local: (f32,f32,f32), energy: f32, level: u8, seed: u32)]`); loading re-stamps the list in
order with the same seeds, which is byte-identical. Pre-damaged glass in a level is the same list.

## Versioning

`format_version` 1. A later bump adds a step to `migrate_glasscrack` and a `check-formats` entry
(`format-hashes.ron` key `glasscrack`). Changing a field without a migrator breaks DO27 content.
