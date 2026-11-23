# `.playlist.ron` (format_version 2)

The dedicated server's playlist (spec 76, ADR-8345): ordered rows of a map, a mode and options.
`server.toml` names the file with `levels.playlist`. That key is exclusive with `levels.rotation`.

```ron
(
    format_version: 2,
    looping: true,
    entries: [
        (map_id: "levels/a.level.ron", mode: "", options: {}),
        (map_id: "levels/b.level.ron", mode: "ctf", options: { "limit": "10" }),
    ],
)
```

| Field | Meaning |
|---|---|
| `format_version` | `2` (`1` still loads, see Versions). Any other value is refused: `no migrator from playlist.ron vN`. The version is read before the rest of the file, so a newer file whose body changed still gets this error, never a RON parse error (ADR-8553). |
| `looping` | After the last row, start again at the first. Default false. |
| `entries` | At least one row, in play order. |
| `entries[].map_id` | Level path the host loads. Never empty. |
| `entries[].mode` | Mode id written to `LevelSettings.game_rules`. Empty keeps the level's own rules. |
| `entries[].options` | String options copied into the `MatchOptions` resource. Default empty. |
| `order` | ADR-8897. `in_order` (the default), `shuffle(seed: N)` (every row once per cycle, a fresh `DetRng` order per cycle) or `weighted(seed: N)` (each row `weight` times per cycle). Not written at its default. |
| `no_repeat_last` | ADR-8897. Avoid the last `n` rows played while another row fits, `0..=255`. Default `0`; a shuffle guards at least the last row. Not written at its default. |
| `entries[].weight` | ADR-8897. Plays per weighted cycle, `0..=100` (above 100 is refused). Default `1`, not written at its default. |
| `entries[].min_players`, `entries[].max_players` | ADR-8897. The host skips the row while the players on the game port are outside `min..=max` (`server.rotation_get` `skipped`). Default none; a minimum above the maximum is refused. Write the number bare (`min_players: 4`) or as `Some(4)`. |

Unknown fields, in the file, in a row or inside `order` (`shuffle(seed: 4, cycles: 2)`, since v2), are refused
with an error that names the field (ADR-8553, ADR-10064): a misspelled `looping` is an error, never a silent
`false`. A later field arrives with a version bump and a migrator.

The ADR-8897 fields are optional and were added to the unreleased v1 before its freeze (the
schema hash is re-recorded with `cargo xtask check-formats --update`): a file without them loads as
before and writes back without them. The `regia.playlist` lobby key is the file name without
`.playlist.ron`.

The host tries each following row once when a row fails to load, and logs `server_next_level_failed` for that
row.

## Versions

| Version | Release | Change |
|---|---|---|
| 1 | v0.6.0 | First version (ADR-8345; the ADR-8897 fields were added before the v0.6.0 freeze). |
| 2 | v0.7.0 (format batch 2) | Same shape. The reader also refuses an unknown field inside `order`, and the schema fingerprint covers the `order` variants and their `seed` payloads (v1's left the enum out, so a rename of `shuffle` or `seed` would not have moved the hash). ADR-10064. |

v2 is frozen with format batch 2: a later change needs a version bump, a migrator, an `upgrade-project` row, a
corpus file and a `format-hashes.ron` row (`docs/specs/12-assets.md` § 4a).

Migration: a v1 file loads as v2 (the loader changes only the number in memory, so saving writes v2).
`regia upgrade-project --write` rewrites the `format_version` line of a v1 file to `2` and keeps every other byte;
a second run changes nothing. A v1 file whose `order` holds an unknown field (which v0.6.0 dropped) is refused with
the field's name: delete the field. The v0.6.0 loader refuses a v2 file with `no migrator from playlist.ron v2`.
Corpus: `playlist-ron/v1.ron` and `playlist-ron/v2.ron` (`crates/regia_agent/tests/fixtures/format-corpus/`); fuzz
target `net.playlist_ron`.
