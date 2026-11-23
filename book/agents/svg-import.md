# SVG import (`editor.svg_report`)

UI icons and logos are authored as SVG. Regia rasterizes them at cook time (ADR-3350): drop `home.svg` under `assets/`
and `regia cook` writes a texture; list it in an `.atlas.ron` and it becomes sharp icon sprites at the 1x and 2x UI
scales. Nothing about an SVG reaches the game at run time; the pak holds pixels. No cooked format is added, so there is no
new format version and no migrator.

## What is imported

Paths, basic shapes, gradients, clip paths, masks, `use` of elements in the same document, opacity, strokes and
dashes, and the `viewBox` / `preserveAspectRatio` mapping of the SVG 1.1 and SVG 2 specifications, drawn by `resvg`.
The result is straight-alpha sRGB RGBA8 (the convention of PNG sources); the UI premultiplies it in linear light
when it uploads, so translucent edges neither darken nor halo.

**Text is refused.** The importer reads no font, so `<text>` could only draw from a machine's fonts and the pixels
would differ between build machines. Convert the text to outlines in the vector editor (every editor has the command)
and import again. A project that needs live text uses the UI text path.

## A standalone `.svg`

The `.meta` `settings` string is RON; every field is optional. Texture keys (`compress`, `max_size`, `usage`, ...) apply as
for a PNG.

| Field | Default | Meaning |
| --- | --- | --- |
| `sizes_px` | none | Longest edge of each raster, pixels. The largest is the cooked texture; the report lists all. |
| `scale` | `Some(1.0)` | Times the document's own size, used when `sizes_px` is empty. |

The cooked texture is one asset (`regia_asset::CookedTexture`, or `CookedImage` with `compress: Some(false)`). For
icons that must be sharp at more than one scale use an atlas.

## Atlas sprites

An `.atlas.ron` sprite may name an `.svg` as its `image`:

```ron
(version: 1, sprites: [
    (name: "home", image: "icons/home.svg"),
    (name: "panel", image: "frames/panel.svg", nine_slice: Some((4, 4, 4, 4))),
])
```

A 24 x 24 `home.svg` cooks to two sprites, `home@24` (1x) and `home@48` (2x), each rasterized at its own size, which is the
`name@pixels` naming `IconAtlas` reads, so the icon picker finds the sharp drawing for the UI scale. `nine_slice` borders
are written for 1x and double for 2x. `grid` on an SVG sprite is an error. The `.atlas.ron` format is unchanged (version 1).

## Limits and refusals

| Limit | Value |
| --- | --- |
| Document | 1 MiB |
| XML nodes | 20,000 |
| Raster edge | 4096 px |
| Raster pixels | 16,777,216 |

Every refusal has a stable code. The cook reports `svg_...: text`; `editor.svg_report` returns it in `data.svg_error`.

| Code | Meaning |
| --- | --- |
| `svg_too_large` | The file is over 1 MiB. |
| `svg_not_utf8` | Not UTF-8 text (a gzip `.svgz` is refused too). |
| `svg_xml` | The XML does not parse, or has a DTD. |
| `svg_too_many_nodes` | Over 20,000 XML nodes. |
| `svg_external_ref` | An `href` that does not start with `#`, a `url()` or a CSS import rule leaving the document, or `<foreignObject>`. |
| `svg_script` | A `<script>`, an `on...` event-handler attribute (`onclick`, `onload`, ...) or a `javascript:` value. |
| `svg_text_unsupported` | A `<text>`, `<tspan>`, `<textPath>` or `<tref>`: outline it first. |
| `svg_image_unsupported` | An `<image>` or `<feImage>`: embedded and linked images are not imported. |
| `svg_parse` | The renderer's parser refused the document. |
| `svg_empty` | No positive width and height. |
| `svg_raster_too_large` | A requested raster is over the edge or pixel limit. |
| `svg_settings` | The `.meta` settings do not parse, a size is 0 or a scale is not positive. |
| `svg_atlas_unsupported` | A `grid` on an SVG atlas sprite. |
| `svg_io`, `svg_bad_params` | `editor.svg_report` only: the file cannot be read, or the parameters are wrong. |

## Agent: `editor.svg_report`

`editor.svg_report { path | text, settings? }` runs the checks and plans the sizes without writing anything:

```json
{ "path": "assets/ui/home.svg", "bytes": 412, "nodes": 6, "intrinsic_size": [24.0, 24.0],
  "raster_sizes": [[64, 64], [32, 32]], "cooked_size": [64, 64], "atlas_scales": [1, 2],
  "error": null, "message": null }
```

`path` and `text` are alternatives (`path` relative to the project, or absolute under the project or the temp dir;
[Request paths](safety.md#request-paths), ADR-7388); `settings` is the `.meta` settings text, for example `(sizes_px: [32, 64])`. A refused
document is an error (`invalid_params`) whose `data.svg_error` is the code above.

The fuzz target `import.svg` (`fuzz.targets`, `fuzz.run`) checks and rasterizes a mutated document.

## Not built

Monochrome icons as multi-channel distance fields (MSDF) are deferred (ADR-3350 (7)): the generator takes font outlines, not
arbitrary paths. Byte-identical output holds per platform: tiny-skia's SIMD paths can round differently on another CPU
architecture.
