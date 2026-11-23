# `.fluidcache` (format_version 1)

A baked particle-liquid run (`regia_fluid::cache`, ADR-2441). It replays without the solver:
`FluidCache::play(frame)` decodes positions with type tags and ages. The engine writes it with
`FluidCache::record` + `to_bytes` (or the agent's `fluid.bake`). `cook` validates a file and
re-encodes it in canonical form. `regia upgrade-project` knows the suffix (`fluidcache`, v1, no
migrations yet).

All numbers are little-endian.

## Header

| Offset | Type | Field |
|---|---|---|
| 0 | `[u8; 4]` | magic `RFLC` |
| 4 | `u32` | `format_version` (1) |
| 8 | `u32` | header length after this field (40 in v1; a reader skips extra bytes) |
| 12 | `u32` | flags: bit 0 = frames carry a whitewater stream; other bits must be 0 |
| 16 | `u32` | frame count (at most 100,000) |
| 20 | `f32` | the solver's fixed step `dt` in seconds |
| 24 | `[f32; 3]` | quantization origin (the domain's minimum corner) |
| 36 | `f32` | quantum: metres per quantization unit (the largest domain extent / 65535) |
| 40 | `u32` | key-frame interval (default 30, never 0) |
| 44 | `u32` | most liquid points of any frame |
| 48 | `u32` | most whitewater points of any frame |

## Frames

Each frame follows the one before it:

| Type | Field |
|---|---|
| `u32` | payload length in bytes |
| `u8` | 0 = key frame, 1 = delta frame (frame `n` is a key frame exactly when `n % interval == 0`) |
| `u64` | solver step |
| `u32` | liquid point count (at most 4,000,000) |
| `count x 3` LEB128 | per point and axis, the zigzag-coded difference of its 16-bit quantized coordinate against the same point index of the frame before (a delta frame where that frame had the index), else against 0 |
| `u32` | whitewater count (only with flag bit 0; at most 4,000,000) |
| per whitewater point | `u8` type tag (1 foam, 2 spray, 3 bubble), LEB128 age in centiseconds, `[u16; 3]` absolute quantized position |

A position is `origin + q * quantum`. Liquid points have tag 0 and age 0 in a decoded `FluidFrame`.

## Reader rules

A reader refuses the file (`CacheError`, codes `fluidcache_*`) when:

- the magic or version is wrong;
- any count is over its cap, or more than the remaining bytes can hold (each coordinate takes at
  least one byte, each whitewater point eight). This check runs before anything is allocated;
- the data ends early, a varint is longer than five bytes, a coordinate leaves `0..=65535`, a tag is
  unknown, a key-frame flag does not match the interval, or bytes are left over in a frame or after
  the last one;
- the header maxima do not match the frames.

## Migration

v1 is the first version. A later version adds its migrator to `regia_fluid::cache::cook` and the
`fluidcache` row of `regia upgrade-project`.
