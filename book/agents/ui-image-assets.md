# UI pictures by asset path or icon (agents)

ADR-3148, `docs/specs/81-ui-designer.md` § 4.6. A `.ui.ron` (format v9) can name a picture without game code: an
`image` or a group `mask` names it by **asset path** (the manifest path of a cooked image or texture) or by **icon
name** (a drawing in the `IconAtlas`). The raw renderer `id` stays for code that registers images at run time.

```ron
(version: 9, root: (children: [
    (name: "logo", style: (size: (128, 64)), image: (asset: "assets/ui/logo.png")),
    (name: "settings", style: (size: (24, 24)), image: (icon: "gear", tint: "#c0c0c0ff")),
    (name: "panel", style: (size: (300, 120)),
        image: (asset: "assets/ui/panel.png", nine_slice: (12, 12, 12, 12))),
    (name: "portrait", style: (size: (96, 96)), mask: (icon: "ring", fit: contain),
        children: [(image: (id: 7))]),
    (name: "badge", style: (size: (48, 48)), mask: (asset: "assets/ui/star.png")),
]))
```

- `image` names at most one of `id`, `asset` and `icon`; `mask` exactly one of `image`, `asset` and `icon`. An empty
  name, two names or a negative inset is refused when the file loads.
- `nine_slice: (left, top, right, bottom)` in source pixels draws the picture as the node's nine-slice background:
  the corners keep their size, the edges and the middle stretch. `tint` and `preserve_aspect` then do not apply.
- An icon picks the drawing whose size best fits the node's laid-out height (the icon scaling rules,
  [UI icons](ui-icons.md)).

## At run time

`UiPlugin` adds the loader (`regia_ui::ui_image_assets`). An asset path resolves through the asset server's manifest
(`assets/ui/a.png`, `ui/a.png` and `assets://ui/a.png` name the same file); the cooked file is read and decoded on the
async task pool, so a frame never waits for it. Boot warm-up uploads pictures already queued, including the
built-in placeholder and fallback, so their bind groups exist before the first overlay draw.

1. While it loads, the node draws a faint grey **placeholder** (`UI_IMAGE_PLACEHOLDER`) and its handle says
   `loading`. A mask leaves the node unmasked until its picture is ready.
2. When the load lands (a cooked RGBA8 image is used as is, a block-compressed texture is decoded on the CPU) the
   picture is registered with the UI renderer and the node draws it: `ready`. Every node naming the same asset shares
   one load and one image.
3. **Hot reload**: when the asset server reloads a held asset (its file changed and was cooked again), the loader
   reads it again in the background and the new pixels replace the old under the same image id; the handle's
   `reloads` counts it. A reload that fails keeps the picture already drawn and logs one warning.
4. A picture that cannot load logs **one** warning (per picture and code) and draws the **fallback**
   (`UI_IMAGE_FALLBACK`, a magenta and black checker): `failed` with a code. Nothing panics.

The node holds a `UiImageHandle` (masks a `UiMaskHandle`): the asset id, the renderer image, the UV rect, the status,
the error code and the reload count.

| Code | Meaning |
|---|---|
| `ui_image_not_found` | No manifest entry matches the path (not cooked, or a typo). |
| `ui_image_wrong_kind` | The asset is not an image or a texture. |
| `ui_image_load_failed` | The cooked file could not be read or decoded. |
| `ui_image_icon_missing` | No icon of that name is loaded in the `IconAtlas`. |
| `ui_image_no_asset_server` | The app has no asset server, so a path cannot load. |

## Cook and validation

- The cook follows the typed `image` and `mask` fields of every top-level `assets/ui/*.ui.ron` (the ones the runtime
  loads at start): a build profile's pack contains a texture even when a layout is its only reference. A layout's
  component `use` paths ship too.
- `regia assets validate` adds each layout under `assets/` to the asset set with the paths it names. The validator
  `ui_image_missing` fails a layout that names a picture path no cooked asset matches (finding code `ui_image_missing`, one
  per path) and one that does not parse (`ui_layout_invalid`). The pictures a layout names count as reachable for the
  orphan check. Icon names and renderer ids are not assets and are not checked here.

## In the editor

The UI designer's inspector has **Picture** rows for the selected node's `image` and `mask`: a picker of the
project's pictures under `assets/` (`png`, `jpg`, `jpeg`, `tga`, `bmp`; **Refresh** reads them again) and of the icon
atlas's names, the name as text (empty removes the field), nine-slice insets and the mask fit. Each change is one undo
step. The schematic preview draws the picture from the project's source file; the themed preview (GPU) registers the
decoded source file with its own renderer. In the previews an icon draws the placeholder (the designer has no icon
pages) and a path with no readable file draws the fallback; the document itself is never changed for a preview.

## Methods

| Method | What it does |
|---|---|
| `ui.image_assets { status? }` | Every node whose `image` or `mask` names an asset or an icon: `{ nodes: [{ entity, name, field, kind, source, nine_slice, state: { asset, image, uv, status, code, reloads, nine_slice_background } }], loads_started, loads_finished, failures, reloads, in_flight, assets }`. `status` (`loading`, `ready`, `failed`) filters; another value is refused with `data.reason` `ui_image_status_unknown`. Read-only. |
| `editor.ui_designer_images {}` | The open document's references with `found`, the asset paths it names, how many are `missing`, and the picker lists (see [UI designer](ui-designer.md)). |
| `editor.ui_designer_edit { op: "image" \| "mask", node, asset \| icon \| id, … }` | Name a node's picture (see [UI designer](ui-designer.md)). |

## Example

```json
{"method": "editor.ui_designer_edit", "params": {"op": "image", "node": "#logo", "asset": "assets/ui/logo.png"}}
{"method": "editor.ui_designer_images", "params": {}}
{"method": "ui.image_assets", "params": {"status": "failed"}}
```

Wait for pictures to be ready with `wait_until` on `ui.image_assets` (`in_flight` 0), or step a few frames: the loads
finish on the task pool, and the next update applies them.
