# Cooked extension record: `extensions.ron`

Written by `regia cook` and `regia package` when the project resolves at least one extension, and removed when it resolves
none (spec 57 § 4.4, ADR-1885). Two copies: inside the paks, stored byte for byte at the manifest path
`ext://regia/extensions.ron`, and at `build/paks/<platform>/extensions.ron` (atomic write). Rust:
`regia_framework::extension::cooked`.

```ron
(
    format_version: 1,
    resolved_hash: "5f0c...",
    extensions: [
        (id: "dialogue", version: "1.4.2", manifest_hash: "a31e..."),
    ],
)
```

| Field | Meaning |
|---|---|
| `format_version` | 1. A higher value is `manifest_too_new`; the file is never partly read. |
| `resolved_hash` | The resolver's hash of the set. |
| `extensions` | The resolved set in load order: id, version, hex hash of the manifest bytes. |

`ExtensionPlugin` compares ids, order, versions and manifest hashes with the set it resolved: at start from the file beside
the paks, and otherwise from the pak copy on the first frame the `AssetServer` exists, which is how a packaged game does it on
every platform. A difference is the error diagnostic `extension_set_mismatch`, with both sets in the message, also logged at
error level. A record that does not parse is `manifest_invalid`. `regia package` also compares the file with the current set
to decide whether the paks are stale.

## Versioning

Version 1 is the first. A later shape bumps `format_version` and ships a migrator with `regia upgrade-project`
(`docs/specs/12-assets.md` § 4a). The file is derived data: deleting it and cooking again is always safe, so no migration
of old files is needed for version 1.
