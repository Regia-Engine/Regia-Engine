# FPAK — Regia Engine Pak Archive Format

Version 1. Spec: `docs/specs/12-assets.md` § 5.

## Overview

FPAK is the shipping archive format for cooked assets. A single `.fpak` file contains asset blobs, a
table of contents (TOC), and a path-to-id manifest, all optionally compressed with lz4.

## File layout

```
Offset  Size   Field
0       4      Magic: b"FPAK"
4       4      Format version (u32 LE, currently 1)
8       4      Flags (u32 LE)
12      8      TOC byte offset from start of file (u64 LE)
20      8      TOC byte length (u64 LE)
28      32     Content hash: blake3 of bytes [0..TOC_offset) (u8×32)
60      *      Asset data area (each blob aligned to 4096)
TOC     *      TOC: postcard-encoded `PakToc` struct
```

### Flags

| Bit | Name            | Meaning                          |
|-----|-----------------|----------------------------------|
| 0   | `COMPRESSED`    | Asset blobs use lz4 block compression (256 KiB blocks) |
| 1   | `TOC_COMPRESSED`| (Reserved for future use)        |

A v1 reader refuses a header with any bit other than `COMPRESSED` set (`PakError::UnsupportedFlags`,
ADR-2394): a reserved or unknown bit means a layout it cannot read.

### Asset blob compression

When `COMPRESSED` is set, each asset blob is stored as:

```
Offset  Size   Field
0       N×4    Block offset table (u32 LE, one entry per 256 KiB block of uncompressed data)
N×4    *      Compressed blocks: each is `lz4_flex::compress_prepend_size(block)`
```

To decompress: read the block offset table, then for each block read `decompress_size_prepended`
and concatenate.

## Table of contents

The TOC is a `postcard`-serialized `PakToc` struct:

```rust
struct PakToc {
    entries: Vec<PakEntry>,
    manifest: Vec<ManifestEntry>,
    dependencies: Vec<String>,
}

struct PakEntry {
    id: AssetId,              // u128 GUID
    type_tag: TypePathHash,   // u64 FNV-1a of type path (engine crates hashed in the legacy spelling, ADR-1429)
    offset: u64,              // byte offset from start of file
    size: u64,                // uncompressed byte length
    compressed_size: u64,     // byte length on disk
    block_table_offset: u64,  // 0 when stored uncompressed
}

struct ManifestEntry {
    path: String,       // project-relative, `/` separators
    id: AssetId,        // u128 GUID
    type_path: String,  // Rust type path for reflection
}
```

## Mount order

Multiple paks mount in list order. A later pak overrides an earlier one when both contain the same
`AssetId`. The dependency list in the TOC records which paks must be mounted first.

## Runtime mounting (automatic)

No game code configures paks. `AssetServerSettings::default()` detects the content directory at
startup:

- **Desktop**: `<exe dir>/content` when that directory exists — exactly the layout
  `regia package` produces (`<name>/{binary, content/*.fpak}`). Dev runs (no `content/` next to
  the executable) read the dev cooked cache (`.regia/cooked/<platform>/`) unchanged.
- **Android**: the umbrella `DefaultPlugins` copies `content/*.fpak` out of the APK assets
  (`AAssetManager`, subdirectory `content`) into `<internal data path>/content` at startup and
  mounts that directory; the copy is skipped when the assets are absent.

Loads try mounted paks first (reverse mount order), then the dev cooked cache. The
`REGIA_PROBE_ASSET=<project-relative path>` environment variable makes `AssetPlugin` load one
mesh at startup and log its source (`regia_asset` target):
`probe: <path> loaded from pak "<name>"` / `loaded from dev cache (not a pak)` — or
`probe: <path> FAILED to load: …` plus an `AppExit::Error(1)` when the asset cannot be loaded. The
packaging end-to-end test (`crates/regia_cli/tests/package_e2e.rs`) uses this to prove a packaged
game serves assets from its paks and fails without them.


## Platform considerations

- **Desktop**: `memmap2` the whole file; decompress on demand on the IoPool.
- **Android**: Read the file into memory via `AAsset`; then use the same decompression path.

## Byte-identical loads

When an asset is loaded from a pak, the decompressed bytes must match the bytes loaded from the
dev cooked cache (the `.regia/cooked/<platform>/<id>.fckd` file). This is tested by the
round-trip test suite in `regia_asset::pak` and `regia_asset_pipeline::pak_writer`.

## Versioning

- Version 1: initial format. Asset blobs aligned to 4096. lz4_flex block compression with 256 KiB
  blocks. Postcard-encoded TOC. blake3 content hash.
- Future versions will bump the version field; the runtime rejects paks with an unknown version.