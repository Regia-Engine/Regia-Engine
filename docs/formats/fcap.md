# `.fcap` — local reflection capture bake (binary)

Owner: `regia_asset::local_capture_bake` (ADR-2860). Current `format_version`: **1**.

A `.fcap` holds every baked local reflection capture of a level: where each capture sits, how it
blends, the share of its directions that saw a surface, and its GGX-filtered equirect map exactly as
the renderer uploads it. A level names its bake in `LevelLook::captures` (scene format v13,
[`scene-files.md`](scene-files.md)); when the level opens, `regia_framework::local_capture_level`
reads the file and `regia_render::sync_local_captures` places every saved `LocalCapture` with the map
baked at its position, so no capture face is rendered. A capture moved since the bake (more than
1 mm from the centre it was baked at), or one the file lacks, is baked by rendering as before.

Written by `render.save_local_captures` (agent) and `regia bake-captures` (CLI, through a running app);
read by the level plugin, `render.load_local_captures` and `regia bake-captures --check`.

## Layout

All integers and floats little-endian.

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `b"RFCP"` |
| 4 | 4 | `format_version` (u32) |
| 8 | 8 | payload length `n` (u64) |
| 16 | 4 | CRC-32 (IEEE 802.3, as zip and PNG) of the payload |
| 20 | n | payload |

Payload:

| Field | Type | Meaning |
|---|---|---|
| `width` | u32 | Width of mip 0 in texels (the height is half of it); a power of two from 2 to 4096. The renderer installs only 512-wide chains. |
| `mips` | u32 | Mips per capture, 1 to log2(`width`) + 1. The renderer installs only its own chain length. |
| `count` | u32 | Captures, at most 64. |
| captures | `count` × record | |

Capture record:

| Field | Type | Meaning |
|---|---|---|
| `id` | u64 | The `LocalCapture` component's id; unique in the file. |
| `center` | [f32; 3] | World centre of the box: the point the six faces were rendered from. |
| `half_extent` | [f32; 3] | Half extents of the parallax box, metres, each > 0. |
| `weight` | f32 | Blend weight against the environment, 0 to 1. |
| `fade` | f32 | Fade distance outside the box, metres, ≥ 0. |
| `coverage` | f32 | Share of directions (by solid angle) that saw a surface, 0 to 1. |
| mips | `mips` × (u32 length, bytes) | Mip `m` is `max(width >> m, 1)` × `max((width / 2) >> m, 1)` texels of `Rgba16Float` (8 bytes): premultiplied radiance over the sky IBL scale in rgb, surface coverage in alpha. Mip 0 first. |

## Refusals

A reader checks, in order: the magic (`fcap_bad_magic`), the version (`fcap_version`), the header and
the declared payload length (`fcap_truncated`), the CRC (`fcap_crc`), the sizes (`fcap_layout`: width,
mip count, mip byte lengths, capture count, trailing bytes) and the values (`fcap_value`: non-finite or
out-of-range numbers, a repeated id). A file IO failure is `fcap_io`. A level whose bake is refused logs
one warning, reports the code in `render.get_capture_bake` `level.error_code`, and bakes its captures by
rendering. A missing file is `captures_not_found`.

## Versioning

v1 is the first version; there is no migrator yet. A future version ships a migrator from v1 and an
`upgrade-project` step (`specs/12` § 4a). `regia upgrade-project` reads the version of every `.fcap` (the
`RFCP` header) and reports a newer one.
