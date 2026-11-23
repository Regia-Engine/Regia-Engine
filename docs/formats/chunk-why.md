# Chunk reasons (`chunk-why.bin`, format `chunk-why` v1)

Spec: `docs/specs/40-content-chunks-dlc-asset-packs.md` § 2.2 step 5, § 5 (`content.why`).
Decision record: ADR-1876. Guide: `book/agents/content-chunks.md`.

Written by `regia cook` / `regia package` next to `chunk-map.bin` (`build/paks/<platform>/`) and
staged with the manifest into the package's `content/` (and `dist/<platform>/chunks/`). It records
why the cook's assignment put each asset in its chunk, so a running game answers `content.why`
without the asset pipeline. The runtime reads it on the first `content.why`, never at startup.

Layout: the 8-byte magic `RGCHWHY\0`, the version as a little-endian `u32`, then an lz4 block
(`lz4_flex` size-prefixed) holding the postcard encoding of:

```rust
ChunkWhy {
    chunks: Vec<String>,                       // chunk names, sorted; records index into this
    selectors: BTreeMap<String, Vec<String>>,  // per chunk rule: its include globs, then "tag:<tag>"
    records: Vec<WhyRecord>,                   // one per cooked asset, sorted by id
}
WhyRecord {
    id: AssetId,
    path: String,                              // "assets/..."
    chunk: u32,                                // index into chunks
    reason: WhyReason,                         // Rule(selector index) | Follows(referrer id) | Shared | Pinned | Default
    duplicated_in: Vec<u32>,                   // shared = "duplicate": every chunk holding a copy
}
```

- `Rule(i)`: `i` indexes the chunk's `selectors` (its `include` globs first, then its `tags`).
- `Follows(id)`: the first referrer on the shortest path from a direct asset (ties by smaller id);
  following the referrers ends at a `Rule` record (`content.why` stops on a cycle).
- The table is deterministic: the same registry and rules give the same bytes.

Version rules: additive changes keep v1 only if old readers still decode the postcard body, which
postcard does not allow for new fields, so any shape change bumps `format_version`. The file is
derived data: an older or missing file only loses the reason chain (`content.why` then answers the
chunk from `chunk-map.bin` with `reason: null`); recooking writes the current version. A newer file is
refused with `manifest_version` and logged; no migrator is needed.
