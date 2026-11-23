# Lobby keys (`regia.*` lobby data, layout version 2)

Owner: `regia_net::lobby` (spec 89 § 4). The key/value data the engine sets on a platform lobby (local, Steam). Not a
file: the layout is what a client of another build reads from a lobby, so it is versioned like one. Game keys must not
start with `regia.` (`RESERVED_PREFIX`).

| Key | Value |
|---|---|
| `regia.v` | The layout version, `2` (`LOBBY_KEYS_VERSION`; `1` before ADR-8897) |
| `regia.game` | The game id |
| `regia.protocol` | The protocol hash, 16 lowercase hex digits |
| `regia.content` | The content hash (from the session's `ContentManifest`), 16 lowercase hex digits |
| `regia.server` | Where the launched session is: `udp:<ip:port>` or `steam:<id>` (id above 0) |
| `regia.state` | `open`, `launching` or `in_game` |
| `regia.ready` | Member data: `1` when the member is ready |
| `regia.mode` | Optional (ADR-8897): the running mode, the current playlist row's `mode` (rule-set id or mode string), else the board's mode lobby id |
| `regia.playlist` | Optional (ADR-8897): the playlist id (`Playlist::id`); absent when the playlist has none |

Limits (Steamworks): a key of at most 255 bytes, a value of at most 8192 bytes, 64 keys per lobby and per member.

Example (as the fuzz target writes it, a RON map):

```ron
{
    "regia.content": "fedcba9876543210",
    "regia.game": "arena",
    "regia.protocol": "0123456789abcdef",
    "regia.ready": "1",
    "regia.server": "steam:76561198000000001",
    "regia.state": "open",
    "regia.v": "1",
}
```

**Layout v2 (ADR-8897)** appends the two optional keys `regia.mode` and `regia.playlist` to `ALL`. No
other key changed, so the v1 → v2 step rewrites nothing: a v1 lobby reads unchanged (it has neither
key), and a v1 build ignores the two keys. `lobby_keys_migration_sources()` lists v1. Corpus fixtures:
`lobby-keys/v1.ron` and `lobby-keys/v2.ron`.

**Versioning.** `regia_net::lobby::keys::ALL`, `STATES` and `SERVER_PREFIXES` are the layout; `cargo xtask
check-formats` hashes them (`lobby-keys`, ADR-2316), so renaming a key without bumping `regia.v` fails the gate. Not a
file, so `regia upgrade-project` has no entry. The fuzz target is `net.lobby_keys`.
