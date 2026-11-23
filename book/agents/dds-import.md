# DDS import: cube maps, arrays, BC6H, signed BC4 and BC5

Skies and reflection captures from DCC tools arrive as DDS cube maps, HDR cube maps as BC6H and tangent-space normal
maps as BC5 SNORM. The `.dds` importer (spec 95 § 3, ADR-3266) reads them. There is no new agent method: the cook and
`editor.import` take the file like any other texture, and `assets.cook` reports its warnings and errors.

## What cooks to what

| Source | Cooked texture |
|---|---|
| Cube map (DX9 `caps2` cube + six face flags, or DX10 cube flag) | One KTX2, `faceCount` 6, faces in the order +X, -X, +Y, -Y, +Z, -Z, the file's mip chain kept |
| Cube array (DX10 cube flag, `arraySize` > 1) | `faceCount` 6, `layerCount` = number of cubes |
| Texture array (DX10 `arraySize` > 1) | `layerCount` = array size. An `arraySize` of 1 is a plain texture |
| BC6H UF16 / SF16 (2D, cube or array) | Desktop: blocks pass through as `BC6H_UFLOAT` / `BC6H_SFLOAT`. Android: decoded to float and stored as `R16G16B16A16_SFLOAT` (signs kept; 4x the BC6H size, the same warning as an HDR image source) |
| BC4 / BC5 SNORM (2D) | Decoded, remapped to unsigned (`v * 0.5 + 0.5`; BC5 gets Z rebuilt), then cooked like any normal map or mask: UNORM BC5 / BC4 on desktop, ASTC on Android |
| BC1/BC3/BC4/BC5/BC7 cube or array | Desktop: blocks pass through (the DX10 `_SRGB` / `_UNORM` format picks the color space). Android: each face is transcoded to ASTC |
| BC2, BC1 with punch-through alpha, uncompressed 32-bit, signed BC4/BC5 as a cube or array | Decoded to RGBA8 and stored uncompressed (`R8G8B8A8_SRGB` or `_UNORM`) with the file's mips |

The renderer's material loader has no signed BC format, which is why signed data is remapped instead of passed through.
A volume (3D) texture is refused: `dds_unsupported: volume (3D) textures have no cooked form; export the slices as a
texture array or an atlas`.

## `.meta` settings

No new key. These existing ones apply: `usage` (a BC5 SNORM file defaults to `normal_map`, BC4 SNORM to `mask`),
`max_size` (drops leading mips, always keeps the last), `compress: false` (BC data is decoded to uncompressed),
`quality` (the ASTC transcode on Android), and `hdr_encoding` (`auto` = BC6H on desktop and RGBA16F on Android, `bc6h`,
`rgba16f`, `astc_hdr`; ASTC HDR stores no negatives, so an SF16 source clamps them to 0).

## Errors

All start with a lowercase code: `dds_bad_header` (not a DDS file, a wrong size, a non-square cube, a bad mip count or
array size), `dds_unsupported` (a volume, a cube missing a face, another resource dimension, an unknown format),
`dds_truncated` (the file ends before its last image).

## Fuzzing

`fuzz.targets` lists `import.dds` in the editor: a BC1 cube map (4×4 faces, three mips), parsed and cooked for desktop and
Android. `fuzz.run { "target": "import.dds", "seeds": 5000 }` mutates it.
