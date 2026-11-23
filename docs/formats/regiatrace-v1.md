# `.regiatrace`, format version 1

A trace recording (`regia_telemetry::trace`). Design: `docs/specs/54-gpu-breadcrumbs-and-trace-markers.md`
(ADR-1671). Binary, little-endian, chunked and append-only: a truncated file reads up to its last complete
chunk. A diagnostic output, not project content: `regia upgrade-project` never rewrites it; a layout change bumps
the version and the reader keeps a decoder for every older one (`crates/regia_telemetry/tests/data/v1.regiatrace`
must always open). A reader refuses a newer version with a clear error.

```
Header  { magic "RGTR", format_version u32 = 1, flags u32, start_unix_ms u64, gpu_clock_error_ns u32,
          engine_version, adapter, build_profile, platform }          // each string: u16 length + UTF-8
Chunk*  { kind u8, len u32, payload [len], crc32 u32 }                // CRC-32 of the stored payload
        kind high bit (0x80) = payload is an LZ4 block with a 4-byte length prefix (used above 1 KiB when smaller)
Kinds   1 Strings  (id varint, len varint, utf8)*      ids start at 1; appended before first use
        2 Lanes    (id varint, kind u8, n varint, name sid)*     kind 0 main, 1 render, 2 worker n, 3 gpu
        3 Events   batch: kind u8, t_ns delta varint, lane varint, fields (below)
        4 Counters name sid, t_ns varint, f64
        5 Passes   (tag varint, name sid)*
        6 Screenshot t_ns, frame, width, height, name sid (0 = none), file sid      (varints)
        7 Index    (second varint, file offset varint)*     written at close
Events  1 Mark{name sid, data sid or 0, frame}   2 RangeOpen{id, name sid, frame, depth, parent+1 or 0}
        3 RangeClose{id, frame, forced}   4 FrameBegin{frame}   5 FrameEnd{frame}
        6 PassOpen{tag, frame, submission}   9 PassClose{...}   7 GpuInterval{tag, begin_ns, end_ns, frame}
        8 Log{level, target sid, text sid}
```

Unknown chunk kinds are skipped by `len`. Times are monotonic nanoseconds since process start. Screenshot PNGs sit
beside the trace in `<trace>.shots/<n>.png` and are referenced by relative path, never inlined.
