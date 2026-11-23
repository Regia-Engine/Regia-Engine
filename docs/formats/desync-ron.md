# `*.desync.ron` (DesyncBundle, format_version 1, diagnostic)

A lockstep desync bundle (spec 120 § 7.6 and § 12.4, ADR-8455): what an offline replay needs to step a diverged
match again. The host writes one when its probe names a subsystem (`DesyncBundle::from_host`), and a diverged seat
can write its own before it takes the majority's state (`DesyncBundle::from_seat`). `regia net desync-replay
<bundle>` replays it headless through the `DesyncReplay` hook registered for its game and names the first diverging
tick, peer and subsystem. Read with `regia_net::desync::DesyncBundle::from_ron` (or `read`), written with `to_ron`
(or `write_to_dir`, atomically, named `<game>-<host|seatN>-<checkpoint>.desync.ron`).

```ron
DesyncBundle(
    format_version: 1,
    game: BundleGame(
        game_id: "regia.desync_sample",
        protocol_version: 1,
        protocol_id: 1234567890123456789,
        engine_version: "0.6.0",
    ),
    writer: host,
    checkpoint: 120,
    snapshot: BundleSnapshot(tick: Some(90), data: "8f00a1..."),
    frames: [
        BundleFrame(tick: 91, inputs: [
            BundleInput(seat: 0, input: Some("02")),
            BundleInput(seat: 1, input: None),
        ]),
    ],
    peers: [
        BundlePeer(voter: seat(SeatId(0)), rows: ProbeRows(
            names: ["units", "economy", "orders", "rng"],
            rows: [SubsystemRow(tick: 99, world: 1, hashes: [2, 3, 4, 5])],
        )),
    ],
    verdict: Some(DesyncReport(
        checkpoint: 120, voter: seat(SeatId(2)), reference: seat(SeatId(0)),
        first_diverged_tick: Some(100), last_agreed_tick: Some(99),
        subsystem: Some("economy"), subsystems: ["economy"],
    )),
)
```

| Field | Meaning |
|---|---|
| `game` | `GAME_ID`, `PROTOCOL_VERSION` and `protocol_id` (game id, protocol version, content hash and wire version) of the game that wrote it, and the engine version. A replay hook of another game is refused (`desync_bundle_game_mismatch`), of another version too (`desync_bundle_game_version`, naming both versions). |
| `writer` | `host`, or `seat(N)` for a seat's own bundle (voters are written `seat(SeatId(N))` or `observer`) |
| `checkpoint` | The checkpoint of the verdict (a seat's bundle: its newest stepped tick) |
| `snapshot` | The last agreed state: `tick` (`None` = the match start, the first frame is tick 0) and the game's encoding of its state as hex (at most 32 MiB) |
| `frames` | Every frame after the snapshot, tick by tick without a gap (at most 8,192): each listed seat's input bytes as hex (`None` = absent; at most 60,000 bytes), and `membership` (seats that join or leave, omitted when empty). Seats are read in ascending order whatever the file says. |
| `peers` | Each probed simulation's subsystem rows (at most 64 peers, 64 names of at most 64 bytes, 8,193 rows ascending by tick, one hash per name), read in ascending voter order |
| `verdict` | The live probe's report, when the host had one |

**Untrusted input.** The reader refuses text over 64 MiB before parsing, never sizes a buffer from a field of the
file, and checks every count, length and order above; `crates/regia_net/tests/desync_bundle_fuzz.rs` runs 20,000
seeded mutations under a counting allocator. Codes: `desync_bundle_too_large`, `desync_bundle_parse`,
`desync_bundle_version`, `desync_bundle_invalid`, `desync_bundle_io`.

**Versioning.** A diagnostic file, `format_version: 1`; another version is refused with an error naming it (replay a
bundle with the engine version that wrote it). Not shipped content; a layout change bumps the version. Registered in
`cargo xtask check-formats` (`desync`), `regia upgrade-project` and the fuzz-target registry (`net.desync`) by
ADR-8455.
