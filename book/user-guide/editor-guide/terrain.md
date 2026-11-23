# Terrain

The **Terrain** tab shows the terrain the open level names, exports and imports heightmaps, sets
the grass density target and edits scatter rules. The viewport's terrain tool (`T`) sculpts and
paints it; see [Viewport](viewport.md). Depth: [Terrain editing (agents)](../../agents/terrain-editing.md)
and [Level terrain](../../agents/level-terrain.md).

## Create a terrain

**New terrain…** (in the Terrain tab, or **Content ▸ New ▸ Terrain…**, which starts in the shown
folder) opens the New terrain dialog:

| Field | What it sets |
|---|---|
| Name | The terrain's folder and file name (letters, digits, `_`, `-`) |
| Folder | Where the terrain's folder goes, under `assets/` (default `assets/maps`) |
| Size | 129, 257, 513, 1025, 2049 or 4097 samples a side (one fewer cells) |
| Cell size | 0.25, 0.5, 1, 2 or 4 meters between samples; the terrain is `(size − 1) × cell size` meters wide |
| Height range | The heights of the lowest and the highest 16-bit sample; a new terrain is flat at the height nearest 0 m |
| Layers | Layer names, bottom first: grass, rock and dirt when the project has the starter content's terrain layers, else one `ground` layer |
| Start from a 16-bit heightmap | Import as new: the file's samples are kept as they are and its size sets the terrain's |

**Create** writes the terrain by the cook convention (`docs/formats/terrain.md` § 9) into
`assets/<folder>/<name>/`: `<name>.terrain.layers.ron`, `heightfield.png` (16-bit), and
`heightfield.json` (the width). It cooks the terrain in the background (the dialog shows the
progress, and **Cancel** stops it with nothing written), then names it in the level (World
settings ▸ terrain) and builds it, so the terrain tool can sculpt it at once. Save the level to
keep the reference. An unsaved level builds the terrain once it is saved.

**Undo** removes the level's reference and the built terrain. The files stay in the Content
Browser: delete the folder there if you do not want them.

| Action | Control | Agent method |
|---|---|---|
| Create a terrain | **New terrain…** ▸ **Create**, **Content ▸ New ▸ Terrain…** | `editor.terrain_create` |
| Import a heightmap as a new terrain | **Start from a 16-bit heightmap** ▸ **Create** | `editor.terrain_create { heightmap }` |
| Cancel the cook | **Cancel** | `editor.cancel_job { id: "terrain_create:<n>" }` |
| Read the level's terrain | the tab | `terrain.level_terrain` |
| Export or import the heightmap | **Export**, **Import** | `terrain.export_heights`, `terrain.import_heights` |

The dialog's fields take focus with Tab; it has no keyboard shortcuts.
