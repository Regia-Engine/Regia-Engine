# `net-manifest.ron` (NetManifest, format_version 1)

The gameplay content manifest a client must match to join a server (spec 77 § 4.1, ADR-1663). Generated, never
edited by hand: every package has `content/net-manifest.ron` (written by `regia package`, spec 76 § 3, client and server
packages of one tree write the same bytes); `regia server` writes it from the dev cache into `build/server-dev/content/`. Read and written by `regia_net::session::manifest::ContentManifest`.

```ron
NetManifest(
    format_version: 1,
    engine_version: "0.5.0",
    game_version: "1.2.0",
    protocol_version: 1,
    protocol_hash: 0x8f3a51c2d9e04b17,
    content_hash: 0x0123456789abcdef,
    packs: [("base", 0x1111111111111111), ("dlc_harbor", 0x2222222222222222)],
)
```

| Field | Meaning |
|---|---|
| `engine_version`, `game_version` | Version texts (logged; not compared) |
| `protocol_version` | `regia_net::NET_PROTOCOL_VERSION` |
| `protocol_hash` | Hash over the registered replicated components, messages and input types with their codecs. 0 in a packaged file: the CLI cannot see them, so each process stamps its own at startup (ADR-1886 (6)) |
| `packs` | `(logical pack name, pack hash)`, sorted by name. A pack hash covers `(asset path, content hash)` of its hashed-kind assets sorted by path: the file for a raw asset, the source file plus `.meta` for a cooked one, because cooked bytes differ per platform (ADR-1886 (6)). The same asset in another physical pak hashes the same |
| `content_hash` | Hash over every pack (and, in memory, every mod) in name order |

**Hashed kinds** (`regia_net::session::manifest::HASHED_KINDS`): `level`, `prefab`, `netprofile`, `data_table`,
`data`, `config`, `collision`, `heightfield`, `navmesh`. Textures, materials, audio, animation clips and every other
presentation asset are not hashed: a client with another texture pack still joins, and a server pak that lacks
presentation assets writes the same file as the client pak built from the same tree.

**Mods** are added at session start (`mods: [(id, version, hash)]`, in memory only) and never written to the file.

**Mismatch.** The server compares hashes only (constant time in the number of assets) and refuses with
`content_mismatch`; the detail lists up to 32 items sorted by name, one per line, `pack:<name>` or `mod:<id>` with
`missing on client`, `missing on server` or `different`.

**Versioning.** New Regia format, `format_version: 1`. A layout change bumps the version and both sides refuse other
versions with `content_mismatch` (detail `manifest_version`). Registered in `cargo xtask check-formats` and `regia upgrade-project` since ADR-2316 (spec 12 § 4a): any shape change, an added field with a serde default included, bumps `format_version` and ships a migrator (a version rewrite when older files still read).
