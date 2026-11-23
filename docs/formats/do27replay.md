# `.do27replay` — match replay (binary, read-only)

Owner of the layout: the game that writes it (a match replay, versions 2, 3 and 4). Reader:
`regia_framework::replay_sim` (`ReplayFile::parse`, `ReplaySim`), frozen against fixtures by ADR-1378.
Regia reads this format and never writes a version for a game; `SyntheticReplay` is a test writer only.
It is user data, not cooked content, so it has no `regia upgrade-project` migrator.

## Layout (what the reader walks)

Little-endian. Strings are a length prefix and UTF-8 bytes.

| Part | Content |
|---|---|
| magic | `DO27` |
| `version` | u32, **2**, **3** or **4** |
| header (all versions) | `protocol` string, `tick_rate` u32, `lobby_preset` string |
| header (version 3 adds) | `seed` u64, `bootstrap`, `map`, `mode` strings, `teams` u8, player table (`peer_id`, `account_id`, `callsign`, `role`, `team`), `input_delay` u64, peer table (`peer_id`, `role`), `sim_git_sha` string, `sim_abi` u32, `content_hash`, `config_hash` strings |
| header (version 4 adds) | A u16-length UTF-8 label after each player row; after `config_hash`, a u8 count followed by that many u16-length UTF-8 team labels. Validated and discarded, without new public fields. |
| records | repeated until the end: a one-byte tag, then the record body |

Record tags: `0x01` full snapshot, `0x02` applied inputs, `0x03` salvage ledger, `0x04` nav, `0x05` peer
commands, `0x06` Moments, `0x07` abort. V4 additionally accepts and skips `0x08` (map extent:
16 bytes after the tag) and `0x09` (training: tick u32, body length u32, then that many bytes).
Skipped records do not supply an input tick or contribute to playback state, counts, hashes,
events or end tick. Lengths are checked before advancing; body bytes remain opaque.

Other tags end the read with `unknown_tag` and the tag's byte offset. There is no common
record envelope from which to derive an arbitrary tag's length. V2/v3 still reject `0x08`
and `0x09`, preserving their existing behavior (ADR-8191).

The snapshot inside a *full* is opaque to Regia; a game plugs in a `FullCodec` to read it and to recompute the
state hash. Without one the hash check is reported as skipped, not passed.

## Versions

| Version | Change | Reader behavior |
|---|---|---|
| 2 | Short header, a full has no length prefix | Loads when the codec can say where a full ends (`FullCodec::v2_full_len`); otherwise `V2Full` with the byte offset. Version 3 fields read as empty or zero. |
| 3 | The long header above, length-prefixed fulls with a stored state hash | Supported unchanged. |
| 4 | Extra player/team labels; bounded extension records | Labels validated and discarded; `0x08`/`0x09` skipped; known records and full snapshots unchanged. |
| 5 and above, 0, 1 | — | Refused at the version field: `unsupported replay version N (this reader accepts 2, 3 and 4)`, code `unsupported_version`. |

## Limits and errors

At most 512 MiB. Every malformed input is an error and never a panic: `truncated` (with the offset and the
bytes missing), bad magic, `unsupported_version`, `unknown_tag`, bad string, bad Moment, bad nav, `too_large`.

## Fixtures

`crates/regia_framework/tests/fixtures/replay/` holds five frozen files, pinned by length and hash in
`tests/replay_format_compat.rs`. The original v2/v3 fixtures are unchanged. The historical
`v4_future` fixture is a v3 layout stamped v4, not a valid v4 file; it remains rejected as
malformed. `v4_extensions` carries non-empty UTF-8 player/team labels, every existing record,
and opaque extensions before, between and after known records. Future-version rejection is
tested with version 5. A new layout adds a fixture without rewriting the old files.
