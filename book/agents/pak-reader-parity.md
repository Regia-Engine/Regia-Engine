# Pak reader path parity

`regia_asset` tests keep the owned-byte and mapped-file entry paths aligned. The cases include valid raw and
multi-block LZ4 payloads, malformed TOC ranges, entry offsets, duplicate IDs, block-table offsets, declared
lengths and block payloads. For accepted entries, `read_shared` and `read_decompressed` must produce the same bytes.
The tests also cover the sequential/parallel LZ4 block-count boundary, isolation of a corrupt entry, and a raw
shared payload retaining its backing bytes after the reader is dropped.

## Checks

Run the crate suite, including its deterministic fuzz integration:

```sh
cargo test -p regia_asset
```

Run only the parity checks while iterating:

```sh
cargo test -p regia_asset owned_and_mapped_entry_paths_have_security_parity
cargo test -p regia_asset lz4_sequential_parallel_threshold_boundary_round_trips
cargo test -p regia_asset a_raw_shared_payload_from_owned_bytes_outlives_its_reader
```

There is no dedicated agent method or registered `fuzz.run` target for PakReader. The existing agent calls `agent.guide`
with `topic: "fuzzing"`, `fuzz.targets` and `fuzz.run` cover the general fuzz registry; the PakReader mutation coverage
for this lane runs through `cargo test -p regia_asset`, including `tests/fuzz.rs`. The shared `fuzz.run` errors remain:
unknown target `F0003`; invalid iteration counts or paths `F0002`; panic-abort builds `F0016`. This lane adds no agent
method and changes no error codes.
