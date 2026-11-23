# `.rfdrec` recording, format_version 1

A DCC scene feed recording (spec 60 § 4.11): the exact frames a session delivered, each with the
simulation tick of its arrival, so a replay is deterministic. Written and read by
`regia_agent::dcc_feed::recording`.

Little-endian layout:

| Field | Type |
|---|---|
| magic | `"RFDR"` |
| `format_version` | u32, 1 |
| `protocol_version` | u32 (the wire protocol the frames use) |
| `client_name` | string (`u16` length + UTF-8) |
| `dcc` | string |
| `record_count` | u64 |
| records | `u64 sim_tick`, `u32 frame_len`, then the complete wire frame, header included |

Recording taps frames after validation and before coalescing. A fragmented message is stored whole, as one record.
A frame is at least the 12 header bytes and at most 64 MiB of payload after them.

Reading is total: bad magic, truncation, a record count larger than the file can hold, a bad frame
length or trailing bytes are `replay_corrupt`. A `format_version` above 1 is refused as
`replay_corrupt` naming the version. A future version 2 adds a reader that keeps accepting 1; no
project migration is involved (a recording is not project data). Files are written to a temporary
name and renamed into place.

A recording holds one session. Its first record is a `hello` the server synthesizes from the session, so a replay gets the
sender's space and unit scale; a replay takes the client name and space from it. Starting a recording sends a `resync`, so the
file holds the full state. `dcc_feed.replay` injects each record at its recorded tick on a fixed clock.

`regia dcc-feed send --record` writes the same layout from the sending side: its `hello` has an empty token, and because the
client has no simulation, `sim_tick` advances once per clip step or `--watch` change.
