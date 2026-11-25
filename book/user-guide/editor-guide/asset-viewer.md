# Asset Viewer

Panel id: `asset_viewer`.

The **Asset Viewer** tab previews a mesh, a texture or a sound from the project without putting it in
the level. A mesh is drawn in an orbit render with its LOD chain, parts and materials, and optional
overlays; a texture shows its mip chain and one channel at a time; a sound shows its waveform and
plays. Depth: [Editor surface](../../agents/editor.md) (`editor.view_asset`),
[Import settings panel and mesh viewer overlays](../../agents/import-settings.md).

## Open it

Double-click a mesh (`.gltf` or `.glb`), a texture (`.png`, `.jpg`, `.jpeg`,
`.tga`, `.bmp`, `.hdr`, `.exr`) or a sound (`.wav`, `.ogg`, `.flac`, `.mp3`) in the **Content** tab.
The tab is in the default layout's bottom group; **View ▸ Reset layout** brings back a closed one.
Until you open something it says "Double-click a mesh, texture or sound in the Content Browser to view
it." An agent brings the tab forward with `editor.layout_apply {"open": "asset_viewer"}`. There is no
palette command for this tab.

## Look at an asset

1. Double-click the asset in Content. The tab's title row shows its path; a file that cannot be loaded
   shows the reason in red.
2. **Mesh.** The summary gives the parts, triangles, vertices and bounds. Pick **LOD0**, **LOD1** and so
   on to see each level with its triangle count and ratio; the LODs shown are a preview of the cook's
   default chain, not the asset's own cooked ones. Each part lists its material. Drag in the picture to
   orbit. The **Overlays:** toggles **UV0**, **UV1**, **Normals** and **Collision** draw over the render; one that
   cannot be drawn says why in a warning line.
3. **Texture.** The size and mip count show above a **mip** drop-down and a channel selector; pick a mip
   and a channel to isolate it.
4. **Sound.** The sample rate, channels and length show next to **▶ Play**, with the waveform below.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| Open and render an asset as an image with its info | Double-click in Content | `editor.view_asset` (`path`, `lod`, `yaw`, `pitch`, `mip`, `channel`, `overlay`) |
| Mesh overlays | **UV0**, **UV1**, **Normals**, **Collision** | `editor.view_asset` with `overlay` |
| Play a sound | **▶ Play** | `editor.play_sound` |

## Keyboard

The tab handles no keys of its own; its toggles and buttons take Tab focus. Orbiting a mesh is a mouse
drag.

## Known limits

- The mesh LODs are generated with the cook's default settings for the preview; they show what a
  cook with LOD generation turned on would produce.
- The render needs a GPU device; without one the mesh summary shows but no picture.
- The viewer is read-only: it changes no asset.

See also: [Content browser](content-browser.md), [Editor guide](index.md).
