# `release-chunks.json` (release chunks, format_version 1)

Owner: `regia_release_chunks` (ADR-8404). One JSON document, a TSV sibling and a SHA-256 sidecar.
`regia engine chunk cut` and `regia engine chunk pack` write `release-chunks.json`. When `platform`
is set they also write `manifest-<platform>.json`, or `manifest-<platform>.<tier>.json` when `tier`
is set. `regia upgrade-project` owns the whole file name `release-chunks.json`.

This is part of the one v0.6 format bump. It must merge before the v0.6 freeze.

```json
{
  "format_version": 1,
  "platform": "linux",
  "tag": "demo",
  "product_version": "0.6.0",
  "source_commit": "d6ed37677",
  "libc_required": "",
  "tier": "",
  "chunks": [
    {
      "name": "core-000",
      "group": "core",
      "buckets": 1,
      "asset": "core-000.tar.zst",
      "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
      "size": 0,
      "unpacked": 0,
      "files": [],
      "content_key": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    }
  ],
  "links": []
}
```

| Field | Meaning |
|---|---|
| `format_version` | 1. A newer document is refused with `release_chunks_newer`. The message names the version. |
| `platform`, `tag`, `product_version`, `source_commit`, `tier` | Labels. Empty when the caller did not set them. `platform` and `tier` used in a file name must be `[a-z0-9_-]+`. |
| `libc_required` | Optional C library requirement. A consumer that still says `glibc_required` maps that key onto this field. |
| `chunks` | Chunk rows. `size` is the compressed file size the cap applies to. `content_key` is the SHA-256 of the uncompressed tar. |
| `links` | Installer links: `path` should be created as a link to `target`, which is stored in `chunk`. |

Every field is always written. Unknown fields are refused (`manifest_invalid`).

**Chunks.** Each chunk is a deterministic ustar archive, then zstd (`.tar.zst`). The compressed file is at most the configured cap, and the cap is always below 2 GiB. A group that does not fit is split in half until each piece fits. One file that cannot be split is `chunk_over_cap`, and no oversize file is left behind.

**Sidecars.** `release-chunks.tsv` repeats the labels and one `chunk`, `file` and `link` row per record. `release-chunks.json.sha256` lists the SHA-256 of the JSON and of the TSV, two spaces before each base name.

**Versioning.** Registered in `cargo xtask check-formats` (`release-chunks`) and `regia upgrade-project` (ADR-8404). Version 1 is the first version. `migrate_json` re-encodes it. A later bump adds its step there.
