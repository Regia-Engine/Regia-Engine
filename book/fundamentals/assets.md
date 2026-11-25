# Assets and the cook

<!-- index: Cook -->
<!-- index: AssetServer | see: Cook -->
<!-- index: VFS | see: Cook -->
<!-- index: .meta | see: Asset id -->

Source assets (glTF models, PNG and JPEG textures, materials, levels, sounds) live in your
project's `assets/` folder in neutral, editable formats. The **cook** turns the ones the runtime
should not parse itself into compact, platform-specific cooked data. In development the game
reads the cooked cache directly; a packaged game reads the same data from `.fpak` archives. The
code that loads an asset is the same either way.

## Where files live: the VFS

Every engine file access goes through the virtual file system (the `Vfs` resource, from
`PlatformPlugin`) with paths such as `assets://textures/rock.png` or `user://saves/slot1.sav`:

| Mount | Desktop (development) | Packaged desktop | Android |
|---|---|---|---|
| `assets://` | `$REGIA_ASSETS`, else `<exe dir>/assets`, else the crate's `assets/` | `<exe dir>/assets` (raw files copied by the packager) | the APK's assets |
| `config://` | `<assets>/config` | `<assets>/config` | APK `assets/config` |
| `user://` | `<data dir>/RegiaEngine/<exe name>` | same | app internal storage |
| `cache://` | `<cache dir>/RegiaEngine/<exe name>` | same | app cache directory |
| `logs://` | `user://logs` | same | same |

Only `user://` and `cache://` are writable. `Vfs::write_atomic` writes through a temporary file
and a rename, so a crash never leaves a half-written save or settings file.

```rust
use regia::prelude::*;

/// Reads a text file shipped with the game.
fn read_credits(vfs: Res<Vfs>) {
    let Ok(path) = VfsPath::parse("assets://credits.txt") else { return };
    match vfs.read_sync(&path) {
        Ok(bytes) => info!("credits: {} bytes", bytes.len()),
        Err(err) => warn!("no credits file: {err}"),
    }
}
```

## What the cook does

```console
$ regia cook --project my_game
$ regia cook --project my_game --platform android --quality fast
```

| Source | Cooked into |
|---|---|
| `.glb` / `.gltf` | meshes (one per primitive, LODs from `_LOD0…n` naming), materials, textures, skeletons and animation clips |
| `.fbx`, `.obj` | the same outputs, after conversion to an in-memory glTF. FBX and OBJ settings are on [Importing assets](../user-guide/importing-assets.md) |
| `.png`, `.jpg`, `.jpeg` | textures: KTX2 with block compression for larger maps (BC on Linux, Windows, and macOS; ASTC on Android), RGBA8 for small ones |
| `.exr`, `.hdr` | HDR textures (linear float): KTX2 with BC6H on Linux, Windows, and macOS, and RGBA16F on Android, a full mip chain, optionally a cube map ([HDR images](#hdr-images)) |
| `.material.ron` | a cooked standard material that references cooked textures |
| `.scatter.ron` | the cooked terrain scatter set |
| `.matgraph.ron`, `.matinst.ron` | the cooked material graph / instance |
| `.atlas.ron`, `.tilemap.ron`, `.tmx`, `.tmj`, `.fgiv` | the cooked atlas, tilemap and probe-volume data |

### HDR images

An OpenEXR (`.exr`) or Radiance (`.hdr`) file cooks as a linear float texture, so an HDR sky ships in a
pak and loads like any other texture. The cook:

- replaces NaN and infinite samples with 0 and reports them as one cook warning per file
  (`sky.exr: 3 NaN or infinite sample(s) replaced with 0`); negative samples clamp to 0 and samples
  above 65504 (the largest finite half float) clamp to it; alpha clamps to 0 to 1;
- builds the whole mip chain, each level a 2×2 box filter in linear float;
- stores BC6H unsigned float on Linux, Windows, and macOS (a BC6H texel of a smooth sky stays within 6 % of the texel's
  brightest channel), and uncompressed RGBA16F on Android, where BC6H is not available and ASTC HDR
  is not on every phone.

The image's `.meta` `settings` (RON, the same ones PNG textures use; `usage` does not apply, an HDR
source is always linear) add:

```ron
(hdr_encoding: Some(bc6h), cube_face_px: Some(512), max_size: Some(4096))
```

| Setting | Effect |
|---|---|
| `hdr_encoding` | `auto` (default: BC6H, RGBA16F on Android), `bc6h`, `rgba16f` or `astc_hdr` (ASTC 4×4 HDR, for devices that report it) on every platform. `compress: Some(false)` means `rgba16f`. |
| `cube_face_px` | An equirect sky is converted at cook time to a six-face cube of this face size, 4 to 8192 (`+X, -X, +Y, -Y, +Z, -Z`). Every cube texel averages a grid of bilinear equirect samples over its footprint, so the faces meet without seams and a small sun does not alias. Unset keeps the 2D equirect image. |
| `max_size` | Larger sources are box-filtered down a level at a time first. |

Errors start with a code: `hdr_decode` (not an image the decoder reads), `hdr_size` (empty, or more
than 64 Mi texels), `hdr_settings` (a `cube_face_px` out of range), `hdr_ktx2` (the writer or an
encoder refused). The cooked file is read on the CPU by `regia_asset::hdr_texture::decode_hdr_ktx2`; the
studio environment takes it by path or id ([Studio environment](../agents/studio-environment.md)).

Terrain heightfields and their layer textures have their own cook path
([Terrain data: cook and stream](../user-guide/terrain-data.md)). Everything else under `assets/`
(levels, prefabs, UI, audio, fonts, config) is read as-is through the VFS and is copied verbatim
into a package. Fonts are the exception to "verbatim": a package carries each TrueType font subset to the
characters your shipped locales use ([Font subsetting](../agents/font-subset.md)).

The cook is **incremental**: each output is keyed by a hash of the source bytes, the import
settings and the importer version, and only stale outputs are rebuilt. `--clean` rebuilds
everything; `--quality fast` turns off pak lz4 compression for quick iteration (textures cook at the quality their `.meta` sets); `--stats`
prints terrain budget statistics. Output goes to `.regia/cooked/<platform>/` with a manifest
that maps project paths to asset ids. `regia preview <asset>` cooks the project incrementally and shows that asset in a
window, the quickest way to check an import.

## `.meta` sidecars and asset ids

The first import of a source writes a `.meta` file next to it. It holds the asset's **id** (a
GUID that never changes, even when the file moves), the import settings, the format versions it
was imported with, and, for content an agent made, **provenance**: which tool, client and session
created it and who reviewed it. Commit `.meta` files with their sources. Levels and prefabs refer
to assets by id, so renaming a texture in the editor does not break the materials that use it.

The provenance policy is set in `Regia.toml` under `[provenance]`; `regia cook --quality
final` can refuse assets that no human has reviewed ([Agent protocol and MCP](../user-guide/agent-protocol.md)).

## Loading cooked assets

The `AssetServer` resource loads cooked assets by project path or id, from mounted `.fpak`
archives first and then from the cook cache:

```rust
use std::sync::Arc;

use regia::prelude::*;
use regia_asset::AssetServer;
use regia_render::{CpuMaterial, CpuMesh, LabVertex, MeshRenderer};
use regia_scene::Transform;

/// Converts a cooked mesh into the renderer's vertex layout.
fn render_mesh(cooked: &regia_asset::CpuMesh) -> CpuMesh {
    let vertices = (0..cooked.positions.len())
        .map(|i| LabVertex {
            position: cooked.positions[i],
            normal: cooked.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]),
            uv: cooked.uvs.get(i).copied().unwrap_or([0.0, 0.0]),
            tangent: cooked.tangents.get(i).copied().unwrap_or([1.0, 0.0, 0.0, 1.0]),
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect();
    CpuMesh { vertices, indices: cooked.indices.clone() }
}

/// Spawns the helmet once its cooked mesh is available.
fn spawn_helmet(mut server: ResMut<AssetServer>, mut commands: Commands, mut done: Local<bool>) {
    if *done {
        return;
    }
    let handle = match server.load_mesh_path("models/DamagedHelmet.glb") {
        Ok(handle) => handle,
        Err(err) => {
            warn!("helmet not cooked yet: {err}");
            *done = true;
            return;
        }
    };
    if let Some(mesh) = server.meshes().get(&handle) {
        commands.spawn((
            Transform::default(),
            MeshRenderer::new(Arc::new(render_mesh(mesh)), Arc::new(CpuMaterial::default())),
        ));
        *done = true;
    }
}
```

- `load_mesh_path`, `load_material_path` and the id-based `load_mesh`, `load_texture`,
  `load_image`, `load_material` return a typed `Handle<T>`; the data is in `server.meshes()`,
  `textures()`, `images()` and `materials()`.
- `server.state(id)` reports `NotLoaded`, `Loading`, `Loaded` or a failure; `server.source_of(id)`
  says whether the bytes came from a pak or the dev cache.
- Every load returns a `Result`. Never unwrap one: a missing or corrupt asset should degrade the
  game, not crash it.

Loads complete synchronously today. Turning a cooked mesh into a `MeshRenderer` is the explicit
conversion above (the cooked mesh is a structure-of-arrays, the renderer's an interleaved vertex
buffer). `regia_render::cooked_model::load_cooked_model` loads the LOD 0 parts of one cooked
glTF by asset id (meshes and materials). Spawning those parts as entities is still your system.
The samples show complete versions: `regia preview` (`crates/regia_cli/src/commands/preview.rs`)
for a static mesh, `samples/third_person/src/assets.rs` for a skinned, animated character.

`regia export usda --glb FILE.glb --out FILE.usda` writes one USDA stage from a glTF binary.
That stage is an exchange file, not a cooked format. The command, its limits, and its error
codes are on [Importing assets](../user-guide/importing-assets.md).

## glTF conventions

glTF is the interchange format for meshes. The importer understands a few naming conventions so
artists can author engine features in their DCC tool: `_LOD0`, `_LOD1`, `_LOD2` suffixes form an
LOD group when `_LOD0` and another level share a stem, `socket_<name>` empties become attachment
sockets, ORM textures are packed occlusion / roughness / metallic, and `KHR_materials_transmission`,
`_volume`, `_ior` and `_emissive_strength` map onto material v3 fields. A file whose meshes are all
`_LODn` with `n > 0` still cooks; the lowest index is the primary. The full list is
[glTF conventions](../agents/gltf-conventions.md).

## Hot reload of assets

Processes built with `regia_asset`'s `dev-assets` feature (the editor, and any dev build that
enables it) watch the cook cache. When `regia cook` or the editor's Reimport rewrites a cooked
file, the server reloads it in place and the next frame uses it.

## Packaging

`regia package` runs the cook when the archives are missing or stale and writes the cooked data
into `content/*.fpak` next to the executable; `AssetPlugin` mounts them automatically on startup
(on Android it extracts them from the APK first). Your loading code does not change. See
[Packaging](../user-guide/packaging.md) and the pak format in `docs/formats/pak.md`.

## Upgrading content

Content formats are versioned. When the engine moves a format forward, its loader still reads
the old version and migrates it in memory, and `regia upgrade-project --write` rewrites your
files on disk for every format with a registered migrator (material, level, prefab, terrain, UI,
theme, VFX and the rest; keeping a `.bak` of each). It finds a file by the same name the cook and
the loader use: a terrain layer stack is any file whose name ends in `terrain.layers.ron`, in any
letter case, so a plain `textures/terrain.layers.ron` is upgraded too. The [Content formats](../reference/formats.md) reference
lists every format, its current version and its migrators.
