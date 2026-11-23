# Install record (`content-state.ron`, format `content-state` v1)

Spec: `docs/specs/40-content-chunks-dlc-asset-packs.md` § 2.5 and § 4. Decision records: ADR-1667
(the format), ADR-7236 (its own `check-formats` row). Guide: `book/agents/content-chunks.md`.

Written by the running game's `ContentManager` into the install directory (the app's private data
directory: `<local data dir>/content` on desktop, `<content dir>/downloads` on Android) whenever a
chunk is installed, verified or released. It records which build each installed chunk belongs to and
the file's hash, so a later start skips re-verifying an unchanged file and marks chunks of an older
build `NeedsUpdate`.

```ron
(
    format_version: 1,
    chunks: {
        "arena": (
            build_id: "1.0-1-0123456789ab",
            hash: "<BLAKE3 hex of arena.fpak>",
            size: 98765432,
            modified: 1700000000,
        ),
    },
)
```

| Field | Meaning |
|---|---|
| `format_version` | `1`. |
| `chunks` | Installed chunks by name. |
| `build_id` | Build the file was installed for; a different running build marks the chunk `NeedsUpdate`. |
| `hash` | BLAKE3 of the installed file (lowercase hex). |
| `size`, `modified` | File length and modification time (seconds since the Unix epoch) when it was verified; both unchanged skips re-verification. Optional (default 0). |

Version rules: a shape change bumps `format_version`. The file is a cache: deleting it is always safe
(every chunk re-verifies), and a newer, damaged or unreadable file is dropped the same way, so no
migrator is needed. `ContentState::from_ron_str` refuses a newer version with `manifest_version`;
`ContentState::read_from_dir` turns any refusal into an empty record.
