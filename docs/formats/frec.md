# `.frec` — match record (binary)

Owner: `regia_framework::input_recording` (RR-7, ADR-1878). Current `format_version`: **1**.

A `.frec` carries exactly the data of a [`.regiademo`](regiademo.md) in a compact, checksummed binary
container: the identity header (engine version, build hash, content hash, seed, level, tick rate), the tracked
actions, one input row and one timestamp list per fixed step, and keyframes with their state hashes and raw
snapshot bytes. `DemoRecording::load_any` reads either form (by the magic); `encode_frec` / `decode_frec`
convert in memory; `InputRecording::to_frec` writes one straight from a recorder's stream.

## Layout

All integers little-endian.

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `b"RFRC"` |
| 4 | 4 | `format_version` (u32) |
| 8 | 8 | payload length `n` (u64) |
| 16 | n | payload: postcard of `FrecPayload` |
| 16 + n | 4 | CRC-32 (IEEE) of the payload |

`FrecPayload` (postcard, fields in this order):

```text
header:     DemoHeader { format_version: u32 (the RON version, 2), engine_version: String,
                         build_hash: String, content_hash: String, seed: u64, level: String,
                         tick_rate_hz: u32, step_count: u64, keyframe_interval: u32 }
actions:    Vec<String>
steps:      Vec<Vec<[f32; 3]>>          // one row per step, one value per action
timestamps: Vec<Vec<u64>>               // empty, or one list per step (µs since recording started)
keyframes:  Vec<{ tick: u64, state_hash: u64, snapshot: Vec<u8> }>
```

The decoded record is validated like a RON demo (step count, row widths, finite values, ascending keyframes
within range, timestamp rows).

## Errors

- File shorter than 16 bytes with a magic prefix, or shorter than `16 + n + 4`: `truncated`.
- Wrong magic, bytes after the checksum, checksum mismatch: `corrupt`.
- `format_version` 0 or newer than this build: `unsupported_version`.
- Payload postcard cannot read: `parse`; inconsistent data: `invalid`. Over 256 MiB: `too_large`.

`save_frec` writes through a temp file and a rename.

## Versions

| Version | Change |
|---|---|
| 1 | First version (ADR-1878). |
