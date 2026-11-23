# UI group opacity, masks and scale modes (agents)

UI parity T11 and Y16, ADR-2866. Three runtime UI features that change how a subtree is composited or how UI
pixels map to the screen:

- **Isolated group opacity.** `GroupOpacity(0.5)` draws the node and its whole subtree into an offscreen layer
  and blends that layer once at 50 %. Two overlapping opaque children show one even tint; with the plain
  `Opacity(0.5)` (which multiplies every primitive) their overlap is visibly darker. A group is also a stacking
  context: a `ZOffset` inside it orders only within the group. Use it for fading whole panels.
- **Rounded clip.** `RoundedClip` (with `ClipChildren`) clips the node's children to its rounded rect (the
  `Border` radius or the theme radius) with an antialiased edge. Pointer hits in the cut-away corners miss.
- **Image mask.** `UiMask { image, uv, fit }` multiplies the node and its subtree by a registered image's alpha;
  `UiMaskAsset { path, fit }` names the image by manifest path instead (see below). `fit` is `stretch` (default),
  `contain`, `cover` or `tile`; outside the placed image everything is hidden. Pointer hits where the mask's alpha
  is zero miss (the hit test reads a CPU copy of the image's alpha, at most 256 texels a side; an image registered as
  a GPU view counts as opaque). An image id nothing is registered under masks nothing.
- **Integer scale.** `ScaleMode::IntegerScale { reference }` picks the largest whole-number scale (at least 1) at
  which the reference resolution fits the window, so every UI pixel is an exact block of screen pixels, and
  centres the layout with a letterbox. For pixel-art UIs.
- **DPI curve.** `UiScaleSettings { dpi_curve: Some(DpiCurve { points }) }` maps the display DPI (96 x the OS scale
  factor) to the UI scale of `ConstantPhysicalSize` roots, piecewise linear between `[dpi, scale]` points. A project
  sets it in its UI settings file (below).

## In code

<!-- ignore-reason: fragment of a startup system; `commands` and the UI components come from the surrounding app -->
```rust,ignore
commands.spawn((UiNode, GroupOpacity(0.5)));                      // fade a panel as one
commands.spawn((UiNode, ClipChildren, RoundedClip,
                Border { radius: [12.0; 4], ..default() }));      // rounded scroll area
commands.spawn((UiNode, UiMask::new(7).with_fit(MaskFit::Contain))); // image 7's alpha
commands.spawn((UiNode, UiMaskAsset::new("assets/ui/star.png")));    // a cooked image's alpha
commands.spawn(UiRoot { scale_mode: ScaleMode::IntegerScale { reference: [320.0, 180.0] }, ..default() });
app.insert_resource(UiScaleSettings {
    dpi_curve: Some(DpiCurve { points: vec![[96.0, 1.0], [144.0, 1.25], [192.0, 1.75]] }),
});
app.insert_resource(UiLayerBudget { max_bytes: 32 << 20 });      // default 64 MiB
```

In a `.ui.ron` file (format v7) the node fields are `group_opacity: 0.5`, `clip_shape: rounded` (or `rect`) and
`mask: (image: 7, fit: contain)` or `mask: (asset: "assets/ui/star.png")`. All three are optional; v6 files load
unchanged. A mask naming both or neither is refused when the file loads.

## Mask images by asset path

An asset-named mask is the manifest path of a cooked image or texture. Since `.ui.ron` v9 (ADR-3148) it loads in the
background like an `image` named by asset path ([UI pictures by asset path or icon](ui-image-assets.md)): the node
draws unmasked until the picture lands (one or two updates after the load finishes; a block-compressed texture is
decoded on the CPU, so the hit test has its alpha), and it stays registered for other nodes naming the same path. A
path that does not resolve logs one `ui_image_*` warning and the node stays unmasked; `ui.set_group` still loads at
once and reports a failure as an error (`ui_mask_asset_failed`). When the asset server hot-reloads the image, the mask
is read again. A mask may also name an icon of the `IconAtlas` (`mask: (icon: "ring")`, `UiMaskIcon`).

## Project UI settings

`assets/ui/project.uisettings.ron` (optional, format in `docs/formats/uisettings-ron.md`) holds the settings a project
wants everywhere:

```ron
(
    version: 1,
    dpi_curve: [(96.0, 1.0), (144.0, 1.25), (192.0, 1.75)],
    layer_budget_bytes: 33554432,
)
```

It is read once at startup and lives under `assets/` so that packaged games carry it (the project manifest does not
ship). A value the game set in code stays. A bad file logs the warning `ui_settings_invalid` and changes nothing.

## Cost and limits

- A group, a rounded clip and an image mask each draw through one pooled layer the size of what the subtree
  covers inside the window (whole device pixels), in `Rgba8UnormSrgb`: 4 bytes a pixel. Layers are pooled in 64 px steps and
  reused across groups and frames; a warm pool allocates nothing (`created_total` stays flat). Slots unused for
  600 window draws are freed.
- No layer is made when it would change nothing: opacity 1 with no mask (`skipped_opaque`), a single primitive
  (its alpha takes the opacity, `skipped_single`), nothing visible (`skipped_empty`), or a rounded clip with square
  corners (it is the rect clip the children already have).
- The boot warm builds the layer pipelines, so the first group compiles nothing.
- `UiLayerBudget` caps the layer bytes of one draw list. A group past it draws non-isolated (every primitive takes
  the opacity; a rounded clip falls back to its rect clip, an image mask to the node's rect) and the warning
  `ui_group_layer_budget` is logged (`over_budget` counts them).
- A backdrop blur inside a layer is skipped (its tint still draws): the frame under the group is not in the layer.
- `GroupOpacity`, `RoundedClip` and `UiMask` on a `UiRoot` entity itself have no effect, on drawing or on hits; put
  them on a child node. `ui.set_group` on a root says so in its reply (`warning: "ui_group_on_root"`).
- Content inside a layer blends in plain linear light; the window's blend-gamma remap applies once, when the layer
  is composited. A single translucent primitive in a group therefore looks the same isolated or not.

## Agent methods

| Method | Params | Does |
|---|---|---|
| `ui.set_group` | `node` / `name` / `text`, `group_opacity?` (0..1, `null` removes), `clip?` (`none`, `rect`, `rounded`), `mask?` (`{image, fit?}` or `{asset, fit?}`, `null` removes) | Sets a node's group, clip and mask; returns `{before, after, changed, warning?}` |
| `ui.layer_stats` | none | `{nodes: {groups, rounded_clips, masks}, plan: {layers, skipped_opaque, skipped_single, skipped_empty, over_budget, bytes}, draw: {drawn, max_depth, mask_passes, pool_slots, pool_bytes, created_total, freed_total, backdrops_in_layers}, budget_bytes}` |
| `ui.set_scale_mode` | `root?`, `mode?` (`constant_pixel`, `constant_physical`, `scale_with_screen`, `integer`), `reference?` `[w, h]`, `match_width?`, `dpi_curve?` (`[[dpi, scale], ...]`, `null` removes) | Sets one root's (or every root's) scale mode and the DPI curve; returns `{roots: [{root, mode, scale, letterbox}], dpi_curve}` |

Error reasons (`data.reason`): `ui_group_opacity_out_of_range`, `ui_clip_unknown`, `ui_mask_image_required`,
`ui_mask_fit_unknown`, `ui_mask_asset_not_found`, `ui_mask_asset_failed`, `ui_reference_invalid`, `ui_reference_required`, `ui_scale_mode_unknown`,
`ui_dpi_curve_invalid`, `ui_root_not_found`, plus the node-selection reasons of `ui.query` (`ui_node_not_found`,
`ui_node_ambiguous`). Log warning codes: `ui_group_layer_budget`, `ui_mask_asset_failed`, `ui_settings_invalid`.

## Verifying

Fade a panel and check that one layer is drawn and nothing is allocated after the first frame:

```json
{"method": "ui.set_group", "params": {"name": "Inventory", "group_opacity": 0.5}}
{"method": "play.step", "params": {"frames": 2}}
{"method": "ui.layer_stats"}
```

`plan.layers` is 1, `draw.drawn` 1 and `draw.created_total` does not grow over later frames. A screenshot shows
the panel's overlapping children at one even tint.
