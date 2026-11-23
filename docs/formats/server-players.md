# `server-players` (format_version 1)

The per-player records a dedicated server keeps (ADR-3354, [server world persistence](../../book/agents/net-server-persistence.md)).
Not a file of its own: a save section inside a world save `<server dir>/<persistence dir>/<level folder>/world-<sequence>.save`
(the save container of [`save-format.md`](save-format.md)), stored as one `SavedResource` whose `type_path` is the
section key `regia_framework::server_persistence::players`. Written by
`regia_framework::server_persistence` on every server persistence save; a game's own saves never carry it.

The payload is postcard, `format_version` first (`PlayerSection`):

| Field | Type | Meaning |
|---|---|---|
| `format_version` | `u32` | 1 |
| `level` | string | The level the save was taken on. |
| `tick` | `u64` | The host tick. |
| `sequence` | `u64` | The save's sequence (its slot; the `world-<sequence>.save` number). |
| `reason` | enum | `Autosave`, `Shutdown`, `LevelChange`, `PlayerLeft`, `Manual`. |
| `players` | list of `PlayerRecord` | Sorted by identity. |

`PlayerRecord`:

| Field | Type | Meaning |
|---|---|---|
| `identity` | string | The stable identity: the authenticator's platform id (`steam:<id>`, `<id>#<index>` for split-screen index 1+), or `name:<name>` under `player_identity = "platform_or_name"`. |
| `name` | string | The player's name when captured. |
| `stamp` | `u64` | The sequence of the save the record went into; on a merge the higher stamp wins. |
| `player` | map string → bytes | Component short type path → typed postcard payload (`ReflectCodec`, as save entities), from the `NetPlayer` entity. |
| `pawn` | map string → bytes | The same, from the player's pawn. |

**Validation.** A `format_version` other than 1 is refused before the rest is read; a record with more than 256
components, or an identity listed twice, is refused. A refused section leaves the kept records as they were and adds
warning `persistence_player_section_unreadable`; the world part of the save still loads.

**Versioning.** New Regia format, v1 (`cargo xtask check-formats` hashes `PlayerSection`, `PlayerRecord` and the reason
names as `server-players`). A save without the section holds no player records: that is how saves from before
ADR-3354 read, so no migrator is needed for them. A later version adds a step to
`server_players_migration_sources`; this section is user data inside a save, so `regia upgrade-project` does not
rewrite it (the server reads older versions at load).

**Fuzzing.** Fuzz target `server-players` (`fuzz.targets`): decode, validate, re-encode.
