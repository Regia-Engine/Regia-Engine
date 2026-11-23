# Content browser

The **Content** tab (first tab under the viewport) is the project's `assets/` folder: a folder
tree and a list or thumbnail grid of assets, with search, create, import, validate, rename,
move, delete (after a check of what refers to the asset) and reimport. Drag an asset into the
viewport to place it; double-click opens it in its editor. Depth: [Content
browser](../../agents/content-browser.md), [Editor asset browser](../../agents/editor-asset-browser.md),
[Editor content index](../../agents/editor-content-index.md), [Asset validators](../../agents/asset-validators.md),
[Importing assets](../importing-assets.md), [Import settings panel and mesh viewer overlays](../../agents/import-settings.md).

| Action | Control / key | Agent method |
|---|---|---|
| Search | The search field | `editor.content_index`, `editor.list_assets` |
| Import files | **Import…** | `editor.import` |
| New material, prefab, level or folder | **New** ▸ Material / Prefab / Level / Folder | `editor.create_asset` |
| List or grid, tile size | **List**, **Grid**, the size slider | `editor.content_view` |
| Validate | **Validate project**; row menu ▸ **Validate…** | `editor.asset_validate` |
| What refers to it, what it costs | Row menu ▸ **Reference graph**, **Size map**, **Audit to CSV** | `editor.asset_graph`, `editor.asset_size_map`, `editor.asset_audit` |
| Rename or move | Row menu ▸ **Rename / Move…** | `editor.rename_asset`, `editor.move_asset` |
| Delete | Row menu ▸ **Delete…** (lists referencers first) | `editor.delete_asset` |
| Reimport | Row menu ▸ **Reimport (forget cache)** | `editor.reimport` |
| Import settings | Row menu ▸ **Import settings…** (images, meshes, sounds): typed fields, **Apply** writes the `.meta` and re-cooks, **Revert** | `editor.import_settings_open`, `editor.import_settings_edit`, `editor.import_settings_apply`, `editor.import_settings_revert`, `editor.import_settings_state`, `editor.set_import_settings` |
| Mesh helpers | Row menu on a `.glb` / `.gltf`: **Add simple collision**, **Generate lightmap UV** (opens the mesh viewer with the atlas), **Build nav preview here**; the same three buttons are in the mesh viewer's Helpers row | `editor.mesh_helper` |
| Mesh overlays | Asset Viewer ▸ **UV0**, **UV1**, **Normals**, **Collision** | `editor.view_asset { overlay }` |
| Content chunk of an asset | Row menu ▸ **Why is this in its chunk?** | `editor.content_chunks_why` |
| Open | Double-click: a level opens in the viewport, a prefab in prefab mode, a material in its editor, a graph in its panel, other assets in the **Asset Viewer** | `level.open` (`editor.open_level` headless), `editor.open_prefab`, `editor.view_asset`, `editor.matgraph_open` |
| Place in the level | Drag into the viewport | `editor.spawn_prefab`, `scene.spawn_primitive` |
| Thumbnails | the grid | `editor.asset_thumbnails`, `editor.thumbnails` |
| Copy path, show in file manager | Row menu ▸ **Copy path**, **Show in file manager** | none (read `path` from `editor.list_assets`) |
| Rescan | **↻** | `editor.content_index` |

The Content tab has no keyboard shortcuts of its own. Its rows and buttons take focus with Tab;
dragging into the viewport has a keyboard-free agent twin but no keyboard twin. Findings:
[Editor accessibility](accessibility.md).

## Complete a search token

In Search, Tab completes a filter prefix or asset type (for example `type:te` becomes
`type:texture`). The candidate queries appear beside the field; press Tab again to cycle them.
Editing the search starts a new candidate list. Shift+Tab leaves the field. Agents can request
these candidates with `editor.content_index { complete: "type:te" }`.
