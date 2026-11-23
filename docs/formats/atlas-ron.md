# .atlas.ron — Frozen sprite atlas v1

Spec: `docs/specs/22-tier2.md` § T2.10. Code: `regia_2d::atlas` (`AtlasSource`, cooked
`SpriteAtlas`), cook: `regia_asset_pipeline::atlas_cook`. Example:
[`atlas.example.ron`](atlas.example.ron).

An `.atlas.ron` lists the images to pack into one sprite atlas. `regia cook` packs them with
`texture_packer`'s skyline packer (deterministic order: tallest, widest, name) into as many pages of
at most `max_size` pixels as needed and writes a cooked `SpriteAtlas`. Version **1**, hashed by
`cargo xtask check-formats`, listed by `regia upgrade-project`.

```
AtlasSource {
    version: u32,          // 1
    max_size: u32 = 2048,  // largest page side (≥ 16)
    padding: u32 = 1,      // transparent pixels between sprites
    extrude: u32 = 0,      // edge pixels repeated outwards (no bleed when scaled/filtered)
    trim: bool = false,    // pack only the opaque bounds; source size and offset are kept
    sprites: [AtlasSpriteSource],
}
AtlasSpriteSource {
    name: String,                     // unique; with `grid`: name_0, name_1, … (row by row)
    image: String,                    // relative to the .atlas.ron
    pivot: (f32, f32) = (0.5, 0.5),   // (0,0) = bottom-left, (1,1) = top-right
    nine_slice: Option<(u32, u32, u32, u32)> = None,   // (left, top, right, bottom) borders
    grid: Option<(u32, u32)> = None,  // cut into cells of this size
}
```

9-slice sprites are never trimmed. A grid cell that is fully transparent becomes a 1×1 transparent
sprite with the cell's source size. Editing any listed image recooks the atlas.

An Aseprite source (`.aseprite`/`.ase`, ADR-3358) cooks through the same packer without an
`.atlas.ron`: each animation tag becomes one horizontal strip sprite (frames left to right, in
playback order) named by the tag, with a slice's 9-slice and pivot attached when it covers the
tag. Clip timing (per-frame durations, repeat counts) is not atlas data; the cook's
`import_reports.json` carries it for the agent method `import.report`
(`book/agents/import-report.md`).

Cooked `SpriteAtlas` (type path `regia_2d::SpriteAtlas`, postcard): pages (uncompressed RGBA8) and
sprites sorted by name, each with `page`, `rect` (packed, trimmed), `source_size`, `trim_offset`,
`pivot` and `nine_slice`.

**SVG sprites (ADR-3350).** A sprite's `image` may be an `.svg`. It is rasterized at cook time at the UI scales 1x (the
document's own size) and 2x and becomes two sprites named `<name>@<height px>` (for example `home@24` and `home@48`), the
`name@pixels` form `regia_ui::icons::IconAtlas` reads; the `nine_slice` borders are written for 1x and double for 2x. No field
changed, so the version stays 1. `grid` on an SVG sprite is an error (`svg_atlas_unsupported`).
