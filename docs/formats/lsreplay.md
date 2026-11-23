# `*.lsreplay` (lockstep match replay, version 1)

A lockstep match replay (ADR-8513): every agreed frame of a match as the bytes each seat sent, the
game's state at intervals (Full records), the checkpoint hashes, the peer commands the host applied
and an abort record when the match aborted. `regia net replay-audit <file>` re-runs it through the
game's `DesyncReplay` hook and checks every recorded hash bit for bit (`book/agents/net-lsreplay.md`).
Written by `regia_net::lockstep::replay::LsReplayWriter`, read by `LsReplayReader` (bounded: every
length is checked against the bytes left and the caps before anything is read or allocated).

Binary, little endian. `str` is a `u16` length plus UTF-8, `str8` a `u8` length plus UTF-8.

```text
magic      8  b"RGLSRPLY"
version    u32  1
header_len u32  (at most 65,536)
header:
  game_id          str (1..=128 bytes)       NetGame::GAME_ID
  protocol_version u32                       NetGame::PROTOCOL_VERSION
  protocol_id      u64                       regia_net::protocol_id::<G>()
  content_hash     u64                       NetGame::content_hash()
  engine_version   str (<= 1,024)            the regia_net version that wrote it (information only)
  tick_rate        u32
  seed             u64
  full_every       u32                       ticks between Fulls
  hash_every       u32                       ticks between Hashes records (0: none)
  seat_count       u16 (<= 1,024), seat u16 * count, ascending
records, to the end of the file:
  kind u8 | len u32 | payload (len bytes; a Full at most 32 MiB + 17, any other record at most 4 MiB)
  0x01 Full      flags u8 (bit 0: the match start, before the first frame) | tick u64 | world hash u64
                 | the game's state bytes (the rest of the payload)
  0x02 Inputs    tick u64 | delay u16 | delay_from u64 | count u16
                 | count * (seat u16 | present u8 | [len u16 (<= 60,000) | input bytes])  seats ascending
                 | membership count u16 | count * (seat u16 | joins u8)
  0x03 Hashes    tick u64 | world hash u64 | count u8 (<= 64) | count * (name str8 | hash u64)
  0x05 PeerCmds  tick u64 | count u16 (<= 256) | count * (seat u16 | ok u8 | kind str8 | text str)
  0x07 Abort     tick u64 | code str8 | reason str
```

Order: the start Full, then per tick its Inputs, the Full when `(tick + 1) % full_every == 0` (the
state after the tick) and the Hashes when `tick % hash_every == 0`; PeerCmds beside the tick they
were applied at; the Abort once, last. A writer at its byte cap sheds Fulls first and then stops at
a whole tick; the abort record is always written (at most 4 KiB past the cap).

Versions: 1 only. A reader refuses a newer version by name before reading its header
(`replay_version_newer`). A recording belongs to its game build, so `regia upgrade-project` reports
`.lsreplay` files and rewrites none.
