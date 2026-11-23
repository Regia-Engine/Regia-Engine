# Size Map

The **Size Map** tab draws what an asset, or a set of assets, costs: every asset the selection
reaches is a rectangle in a treemap whose area is its cooked size. Use it to find the one
texture that makes a level heavy, and to see how many bytes deleting the selection would free. The
panel only reads. Depth: [Asset validators and the dependency graph
(agents)](../../agents/asset-validators.md).

## Open it

The tab is in the default bottom group (**View ▸ Reset layout** restores a closed one); an agent
opens it with `editor.layout_apply {"open": "size_map"}`. It needs the loaded asset registry, so
cook the project first. A fresh panel reads "Use Size map in the Content Browser or in the
Reference graph."

## Walk-through: what makes this level heavy?

1. In the **Content** tab, right-click the level and choose **Size map**. The tab fills in.
2. The line at the top reads `N assets, <total> in total, <shared> shared, <exclusive> exclusive`.
   *Total* counts every reached asset once. *Exclusive* is what removing the selection would free;
   *shared* is what other assets still use.
3. Each rectangle's area is proportional to its size, so the biggest asset is the biggest
   rectangle. Names are drawn on rectangles wide and tall enough to hold them.
4. Hover a rectangle: the tooltip shows the full name, its size with its share of the total in
   percent, and its exclusive size.
5. To go from a big rectangle to the assets that pull it in, right-click it in the Content tab
   and choose **Reference graph** ([Reference Graph](reference-graph.md)).

## What is on screen

| Action | Control / key | Agent method |
|---|---|---|
| Show the size map of an asset | Content tab right-click menu ▸ **Size map**; or **Open size map** in the Reference Graph tab | `editor.size_map_panel` with `assets` |
| Read a rectangle | Hover it | `editor.size_map_panel` with `hover: [x, y]` (and `area: [w, h]` for the layout size) |
| Group rows by asset, kind, folder or tag | none in the panel (it groups by asset; at most 200 asset rows, the rest fold into one `(other)` row) | `editor.size_map_panel` with `group_by` and `top` |
| Write the numbers to a file | Content tab right-click menu ▸ **Audit to CSV** (writes `.regia/audit/<path>.csv`) | `editor.asset_audit` |
| Read the whole panel | the tab | `editor.asset_panels_state` |

## Keyboard

None. The treemap is a hover view; there is no focusable item per rectangle, so use the agent
method or the CSV for a text form of the same numbers.

## Known limits

- It needs a cooked registry; sizes come from the registry.
- The panel does not offer a grouping control; the grouping and `top` choices are agent-only.
- A rectangle's label is hidden when it is too small; hover reads it.

## See also

[Reference Graph](reference-graph.md), [Problems](problems.md), [Content
browser](content-browser.md), [Asset validators and the dependency graph
(agents)](../../agents/asset-validators.md).
