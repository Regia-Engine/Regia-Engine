# Reference Graph

The **Reference Graph** tab answers "what uses this asset, and what does it use?" as columns:
the assets that refer to it on the left (referrers), the asset itself in the middle, and what it
refers to on the right (dependencies), one column per hop. Use it before you delete, move or
rename an asset, and to find out why something ends up in a cook. The panel only reads; it never
changes content. Depth: [Asset validators and the dependency graph
(agents)](../../agents/asset-validators.md), [Content browser](content-browser.md).

## Open it

The tab is part of the default bottom group (**View ▸ Reset layout** brings it back when closed).
An agent opens it with `editor.layout_apply {"open": "reference_graph"}`. The panel reads the
editor's loaded asset registry, so cook the project first; without one it says
`registry_not_loaded: cook the project first` in red. A fresh panel shows "Use Reference graph in
the Content Browser."

## Walk-through: why is this texture in my level?

1. In the **Content** tab, right-click the texture and choose **Reference
   graph**. The Reference Graph tab fills with the asset in the **selection** column.
2. Read the **referrers 1** column on the left: the assets that name the texture directly.
3. Raise the **referrers** value (0 to 8) to follow the chain further back, for example from a
   material to a mesh to a scene. A column holds the assets first reached at that hop.
4. Click any asset name to **re-center** the graph on it. The Content tab also selects that asset
   and opens its folder.
5. Use **<** and **>** to step back and forward through the graphs you have shown.
6. Click **Open size map** to see what the selection costs in bytes ([Size
   Map](size-map.md)).

## What is on screen

| Action | Control / key | Agent method |
|---|---|---|
| Show the graph of an asset | Content tab right-click menu ▸ **Reference graph** | `editor.reference_graph_panel` with `action: "open"` and `asset` |
| Re-center on another asset | Click a name (its hover text says "re-center here") | `editor.reference_graph_panel` with `action: "recenter"` |
| Back, forward | **<**, **>** (grayed out when there is no earlier or later graph) | `action: "back"`, `action: "forward"` |
| How many hops of referrers or dependencies | The **referrers** and **dependencies** drag values, 0 to 8 (default 1 each) | `action: "depth"` with `side` and `depth` |
| How many assets per column | The **width** drag value, 1 to 500 (default 50) | `action: "width"` with `width` |
| Filter by asset type | The **types:** chips (one per type present; with none chosen every type shows) | `action: "kind"` with `kind` |
| Show the rest of a long column | The **N more** button under a column; the last chunk wraps to the first | `action: "more"` with `side` and `hop` |
| Open the size map of the same assets | **Open size map** | `action: "open_size_map"` |
| Read the whole panel | the tab | `editor.asset_panels_state` |

Assets that the selection names but that are not in the registry are listed under **missing:**
below the columns. Reading the same graph without the panel, or with several roots, is
`editor.asset_graph` with `assets`.

## Keyboard

The panel has no shortcuts of its own. Every button, drag value and name is a focusable widget:
Tab moves between them and Enter or Space presses the focused one.

## Known limits

- It needs a cooked registry. Changes you have not cooked are not in it.
- A graph is capped by the width setting: only the assets listed in a column are followed to the
  next hop, so raise **width** or page with **more** to follow a long column.

## See also

[Size Map](size-map.md), [Problems](problems.md), [Content browser](content-browser.md), [Asset
validators and the dependency graph (agents)](../../agents/asset-validators.md).
