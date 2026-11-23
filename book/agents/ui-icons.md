# UI icons (agents)

UI series D16, ADR-865; spec `docs/specs/ui-default-design.md` § 2.7. `regia_ui::icons` is the icon mechanism:
atlases of named drawings at several sizes, the scaling rules, and tinting from the theme. The engine ships no icon
artwork: the default set is an owner choice (spec § 9 Q3, UI D4).

## Loading icons

An `IconSheet` is RGBA8 pages plus named rectangles (`IconSprite { name, page, rect }`).

- `IconSheet::from_grid(page, [w, h], names)` cuts one page into cells, row by row (an empty name skips a cell).
- A cooked `.atlas.ron` sprite atlas (`regia_2d::SpriteAtlas`) maps onto it one to one: each page becomes an
  `IconPage`, each sprite an `IconSprite` with its packed `rect`. `regia_ui` does not depend on `regia_2d`, so the
  host does this conversion; no new content format is involved.

Name a drawing `name@size` (`gear@16`, `gear@24`) to give one icon several drawings; a name with no `@size` is
drawn at its source height. `IconAtlas::add_sheet(sheet)` validates the sheet (`icon_bad_page`, `icon_bad_rect`,
`icon_bad_name`, `icon_duplicate`, `icon_bad_grid`), gives its pages renderer image ids from `ICON_IMAGE_BASE` up,
and adds the drawings. A later sheet's drawing replaces an earlier one at the same size. Pages reach the renderer in
`PostUpdate`.

Draw icons white on transparent: the tint multiplies them.

## Drawing an icon

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# use regia_ui::icons::*;
commands.spawn((UiNode, Icon::new("gear").with_size(IconSize::Small)));
```

- **Size** (`IconSize`): `Small`, `Medium` (default) and `Large` are the theme's `size.icon.sm/md/lg` (16 / 24 / 32
  comfortable, 12 / 16 / 20 compact); `Px(n)` is `n` UI px. The node is laid out at that size unless the author set its
  size (a node whose size is `Auto`, or still the size the icon wrote, follows the icon).
- **Scaling rules**, for the size in physical px (UI size times the UI scale):
  1. a drawing within 1 px of it draws at its own size, texel for texel;
  2. otherwise the smallest larger drawing, scaled down to whole pixels;
  3. otherwise the largest drawing, scaled up, flagged `upscaled` (softer).
- **Tint** (`IconTint`): `Theme` (default) takes the nearest themed ancestor's `icon_color`, else its text color, else
  the theme's `color.text`, so icons follow a theme switch and its cross-fade. `Color(c)` is fixed. `Original` keeps
  full-color artwork.

`IconState` records the outcome on the node: `missing`, `variant_px`, `draw_px`, `upscaled`, `tint`, `ui_px`. A
missing icon draws nothing.

## Method

`ui.icons { entity? }` (read-only, permission `read`).

- Without `entity`: `{ sheets, generation, pending_pages, icons: [{ name, sizes }], nodes: [...], missing,
  upscaled }`. `missing` lists the names of icon nodes whose icon is not loaded; `upscaled` counts soft icons.
- With `entity` (alias `node`): that node's `{ entity, name, missing, ui_px, variant_px, draw_px, upscaled, tint,
  resolved }`.

```json
{"method": "ui.icons", "params": {}}
{"method": "ui.icons", "params": {"entity": 4294967330}}
```

Checks worth automating: `missing` is empty; `upscaled` is 0 at the sizes the game uses.

The handler's lower-case reasons appear in `error.data.code`; malformed ids use JSON-RPC `-32602`, while absent
entities, non-icon nodes and an unavailable atlas use `-32001`, `-32001` and `-32004`, respectively.

## Errors

| Code | Meaning |
|---|---|
| `bad_entity` | `entity` is not a node id |
| `unknown_entity` | the entity does not exist |
| `not_an_icon` | the entity has no `Icon` |
| `ui_not_installed` | the app has no `IconAtlas` (no `UiPlugin`) |

## Workflow

Call `ui.icons {}` to find missing or upscaled names, then use `ui.query` to find an icon node and call `ui.icons` with
its id to inspect the selected drawing and tint.

## Known limits

- `<icon=name>` in rich text still makes a span with the family `icon-name`, not an inline icon: inline icons need
  the text shaper to reserve space for an image (follow-up with D4).
- No signed-distance-field icons: very large icons scale up a bitmap. Ship a large drawing (`@64`) for them.

Captured failure on lavapipe:

Request:

```json
{"jsonrpc":"2.0","id":6,"method":"ui.icons","params":{"entity":0}}
```

Response:

```json
{"jsonrpc":"2.0","id":6,"error":{"code":-32602,"message":"`entity` must be a node id from ui.query","data":{"code":"bad_entity","hint":"omit it to list every icon"}}}
```
