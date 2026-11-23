# glTF conventions for Regia Engine

Published at the start of M3 (`specs/12` § 4a). Frozen with the other content formats in M4. Generators
should target this note now; the importer that enforces it arrives with the M3 hover slice (meshes and
textures) and is completed in M4.

Sources: the glTF 2.0 specification (Khronos) and ADR-009. Neutral industry names only.

## Axes and units

Author files in the glTF 2.0 frame:

- 1 unit = 1 metre
- +Y up
- the front of an asset faces **+Z** (glTF's definition)
- front faces wind counter-clockwise
- materials are metallic–roughness

Regia Engine's world is +Y up and **−Z forward** (`02` § 3). The importer's default `forward: Convert`
bakes a 180° turn about +Y into meshes, node transforms, inverse bind matrices and animation tracks,
so the nose ends up on −Z. Camera and light nodes are compensated so they still look down their local
−Z at the same things. `forward: Keep` does not bake that turn. `.meta` `scale` (default 1) is a
uniform multiplier on the same scene-root matrix either way; a non-finite or non-positive value
cooks as 1. Do not pre-rotate assets in the
generator to compensate.

A Blender generator keeps its current frame (Z up, nose toward −Y) and uses Blender's default glTF
export (+Y up). The file then already faces glTF +Z.

## Node names

| Kind | Name | Rule |
|---|---|---|
| Mesh | anything without a reserved prefix | Imported as a render mesh |
| Authored LOD | same stem plus `_LOD0`, `_LOD1`, … | `_LOD0` is the most detailed. Consecutive indexes from 0. If no `_LOD` suffix is present, the mesh is LOD 0 and lower LODs may be generated at cook (`specs/12` § 7) |
| Socket | empty node, name starts with `socket_` | The rest of the name is the socket id (`socket_turret`). No mesh, or the mesh is ignored |
| Collision | name starts with `collision_` | Authored collision proxy. Not rendered. `collision_box_*`, `collision_sphere_*`, `collision_capsule_*`, `collision_convex_*` and `collision_mesh_*` cook into a box, sphere, capsule, convex hull or static triangle mesh; any other `collision_*` name into a convex hull. The shapes go to the file's `collision` sub-asset ([collision-set.md](collision-set.md), ADR-2341) |

Reserved prefixes are case-sensitive. A node that does not match is an ordinary scene node.

A file whose triangle meshes are all `_LODn` with `n > 0` still imports (ADR-7959). The primary mesh
is the lowest index present, and the first mesh of that index in the default-scene walk wins a tie.
A file that has an `_LOD0` mesh, or a mesh with no `_LODn` tag, keeps that mesh as the primary. A
`LodGroup` is still written only when one stem has `_LOD0` and at least one other level, and the
levels stay sorted by index. Generated LODs still apply only to `_LOD0` or an untagged mesh. A file
with no triangle mesh fails with `no mesh found in glTF file`. A skin and its animation clips cook
with whatever bind mesh is present; that mesh is the clip set's bind mesh.

## LOD screen sizes (v0.1.20, ADR-260)

Each level of an authored chain stores a screen size in its cooked `LodGroup` (`screen_relative_size`:
the projected bounding diameter over the viewport height at 1080 rows at which the level is still
chosen). The cook takes it from, in priority order:

1. **Node extras.** `"extras": {"lod_screen_size": 0.45}` on the `Stem_LODn` node. Inside the GLB, so a
   delivery is self-contained. On a node that is not `Stem_LODn` it is ignored with a warning.
2. **Class table.** `lod_classes.toml` in the project root (next to `assets/`), owned by the game:

   ```toml
   version = 1                      # required; this cook reads version 1

   [classes]                        # class name -> screen size per LOD, LOD 0 first
   battle = [1.2, 0.45, 0.16, 0.05]
   cockpit = [4.0]

   [[assign]]                       # first matching glob wins
   glob = "assets/units/battle/**"  # project-relative path; * within a segment, ** across, ? one char
   class = "battle"
   ```

   The file's class is the root node's `"extras": {"lod_class": "battle"}` (the first root node of the
   default scene that has one), else the first matching `[[assign]]` glob, else an exact class-name
   match to the source filename stem (`assets/props/crate.glb` selects `[classes] crate`, ADR-8054).
   An explicitly unknown class keeps the defaults with a warning; it does not fall through to a
   different class. A level past the end of the class's list uses the default.
3. **Default (ADR-7869).** Halve at each level: 1, 0.5, 0.25, 0.125, 0.0625, … (`2^-n`).
   This gives each level a distinct interval, including LOD3 in a chain of five or more levels.
   There is no lower floor. The helper returns exact binary32 powers through level 149;
   unrepresentable levels >=150 return zero. Supported 16-level chains remain strictly decreasing.

**Validation.** Authored sizes are 0 (last level only) or 0.0001..64. A class must be 1..16 strictly decreasing
sizes; a bad class or rule is dropped with a warning, and a file whose class the table lacks keeps the
defaults with a warning. A chain that used any data is made strictly decreasing: a level that is not
is set to half the previous one, with a warning. The table is capped at 64 KiB, 256 classes and 1024
rules; a wrong `version` or invalid TOML ignores the whole file. Nothing here changes a cooked format;
a file the table reaches keys its cache on the class's sizes, so editing the table recooks it.

Upgrading from v0.5.0 changes fallback thresholds from LOD3 onward (including partial class/extra
fallbacks), and plant exports without explicit sizes write the new defaults. Run `regia cook`
with the upgraded importer before repackaging; the importer revision invalidates local/shared
cooks. No payload layout changed, and `regia upgrade-project` needs no new migrator; that command
alone does not re-cook these assets. Explicitly
authored valid sizes keep priority. Re-export plants to change sizes already written into their
GLB extras: the cooker correctly treats those existing extras as authored values.

## Textures

glTF 2.0 core images, as the specification defines them:

| Texture | Colour space | Channels |
|---|---|---|
| Base colour | sRGB | RGB colour, A opacity |
| Metallic–roughness | linear | G roughness, B metallic. R is ignored |
| Normal | linear | tangent-space, glTF convention (direct X, Y up) |
| Occlusion | linear | R only |
| Emissive | sRGB | RGB |

The cook path packs glTF's separate occlusion (R) and metallic–roughness (G roughness, B metallic)
into one linear ORM image: **R** occlusion, **G** roughness, **B** metallic, **A** 255. Missing
occlusion becomes 255; missing metallic–roughness becomes roughness 255, metallic 0. The cooked
material's `orm_map` points at that packed sub-asset (`texture/orm/{material index}`), not at the raw
metallic–roughness image. Source images are still cooked. A `.material.ron` may also name a file
ending in `_orm.png` with the same channel layout.

Do not pack occlusion into the metallic–roughness texture's R channel and expect the importer to
notice — occlusion stays its own glTF texture, and the cook packs the two maps. Base colour textures
are cooked as sRGB.

## Material extensions (`.material.ron` v3)

The import maps these glTF material fields into the cooked material (`.material.ron` v3); the
runtime bridge (`LabMaterial::from_authored_resolved`) reads them back:

| glTF | `.material.ron` v3 | Renderer |
|---|---|---|
| `metallicFactor`, `roughnessFactor` × metallic-roughness texture B, G | `base.metallic`, `base.roughness`, `base.orm_map`, `base.orm_packed: true` | products clamped to 0–1 (factors outside 0–1 break the spec; they are kept and clamped after the multiply) |
| `emissiveFactor`, `KHR_materials_emissive_strength` | `base.emissive`, `base.emissive_strength` | `emissive × strength × texture`, nits |
| `KHR_materials_ior` | `base.ior` (default 1.5) | F0 = ((ior−1)/(ior+1))² |
| `KHR_materials_transmission` | `transmission.factor` | the frame behind × `factor · (1 − F) · base colour · volume`, then + reflection |
| `KHR_materials_volume` `thicknessFactor`, `attenuationColor`, `attenuationDistance` | `transmission.thickness`, `.attenuation_color`, `.attenuation_distance` (glTF's infinite default is stored as 0) | Beer–Lambert `attenuation_color^(thickness / distance)`, path lengthened by 1 / N·V |
| `alphaMode`, `alphaCutoff` | `render_state.alpha_mode`, `.alpha_cutoff` | mask discard, blend |
| `doubleSided` | `render_state.cull_mode: none` | per material: that material's draws, shadows and motion vectors, including a material graph, use cull-none; `LabScene::double_sided` still turns culling off for the whole frame |

For clear glass with a tint, author a white base colour and put the tint in the volume's
`attenuationColor` / `attenuationDistance`: glTF tints transmission by the base colour too, so a dark
base colour reads as sunglass.

PNG is the interchange format (8-bit colour, 16-bit for heightfields — a heightfield is not a glTF
texture; see `specs/26` § 1).
