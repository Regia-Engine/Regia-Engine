# Snapshot delta codec

`regia_net::snapshot_delta` is a standalone, schema-driven codec for sending world state as
quantised snapshots: a full snapshot first, then per-entity deltas against the last snapshot the
receiver acknowledged. It is a generic engine feature, independent of the ECS replication in
`regia_net::replication`: use it when you already have (or want) plain snapshot values — replay
streams, state-sync prototypes, tools, lockstep-adjacent state broadcasts — and do not want to
mark components for replication.

You describe the per-entity state once as a `SnapshotSchema`, hand whole `WorldSnapshot`s to a
`SnapshotEncoder`, and get back one self-framing packet per call. The peer feeds packets to a
`SnapshotDecoder` and reads the reconstructed snapshot out of each `DecodedSnapshot`.

## The schema

Both peers must build the identical schema. Every packet carries a 32-bit layout hash (field
names, kinds, ranges, widths, variant counts), so a mismatch decodes as `SnapshotError::SchemaMismatch`,
never as silent corruption.

| `FieldKind` | `FieldValue` | Wire cost | Notes |
|---|---|---|---|
| `Bool` | `Bool(bool)` | 1 bit | exact |
| `U8` / `U16` / `U32` | `U8` / `U16` / `U32` | 8 / 16 / 32 bits | exact |
| `I32` | `I32(i32)` | 8–40 bits | zig-zag varint; small magnitudes stay small |
| `Float { min, max, bits }` | `F32(f32)` | `bits` (1–32) | uniform quantisation over `[min, max]`; error at most half a step plus `f32` rounding (`FieldKind::max_quantisation_error`) |
| `Vec3Quantised { min, max, bits }` | `Vec3(Vec3)` | 3 × `bits` | each axis quantised over its own range |
| `QuatSmallestThree { bits }` | `Quat(Quat)` | 2 + 3 × `bits` | dropped largest component rebuilt from the unit norm |
| `Enum { variants }` | `Enum(u32)` | smallest width holding all variants | discriminants must be below `variants` |

Values are quantised **before** comparison, so a change smaller than one quantisation step is not
a change: the per-entity change mask only names fields whose wire bits differ. Non-finite floats
clamp to the nearer end of their range and a zero or non-finite quaternion travels as the identity,
matching the crate's replication codec — degenerate input is defined behaviour, never an error and
never a panic. `SnapshotSchema::quantize_snapshot` (and `quantize_value`) show exactly what a
receiver will reconstruct, which is what tests should compare against.

## Sending and receiving

```rust,wrap
use regia_net::{
    EntitySnapshot, FieldDef, FieldKind, FieldValue, NetId, NetTick, SnapshotDecoder,
    SnapshotEncoder, SnapshotSchema, WorldSnapshot,
};

let schema = SnapshotSchema::new(vec![
    FieldDef::new("alive", FieldKind::Bool),
    FieldDef::new("score", FieldKind::U32),
    FieldDef::new(
        "health",
        FieldKind::Float { min: 0.0, max: 100.0, bits: 12 },
    ),
    FieldDef::new("team", FieldKind::Enum { variants: 4 }),
])?;

let mut encoder = SnapshotEncoder::new(schema.clone());
let mut decoder = SnapshotDecoder::new(schema);

let world = WorldSnapshot::new(vec![EntitySnapshot::new(
    NetId(1),
    vec![
        FieldValue::Bool(true),
        FieldValue::U32(40),
        FieldValue::F32(87.5),
        FieldValue::Enum(2),
    ],
)]);

// Each tick: delta against whatever the client last acknowledged.
let (packet, report) =
    encoder.encode_with_report(NetTick(1), &world, decoder.latest_baseline(), 1200)?;
let decoded = decoder.decode(&packet)?;
assert_eq!(decoded.snapshot.len(), 1);
// Send `decoder.latest_baseline()` back to the server as the ack for the next encode.
let _ack = decoder.latest_baseline();
let _sent_some = !report.included.is_empty();
```

`SnapshotEncoder::encode(tick, &world, ack, budget_bytes)` is the same call without the
`EncodeReport`. The decoder's answer reports the packet's sequence, tick and baseline, the
reconstructed `snapshot` (entities sorted by id), and which entities were `added`, `updated` or
`removed`. Priorities are encoder-local and do not travel; reconstructed entities report priority 0.

## Baselines, loss and the ring buffer

Every packet gets a sequence number (`Seq`, from 1) and names the baseline it was encoded
against: the last sequence the client acknowledged. Both sides keep a ring buffer of recent
baselines keyed by that sequence (`DEFAULT_BASELINE_CAPACITY` = 64; size it with
`with_capacity` to your worst round trip in send ticks).

- **Lost packet:** costs nothing. The client keeps acknowledging the older sequence, so the next
  packet is a delta against the state the client still holds.
- **Baseline evicted at the encoder:** the next encode is a full snapshot (`EncodeReport::is_full`).
- **Baseline evicted at the decoder:** `decode` returns `SnapshotError::BaselineMissing` and
  leaves the decoder's state untouched; keep acknowledging `latest_baseline()` and the server
  will re-base or go full.

Always acknowledge `SnapshotDecoder::latest_baseline()`: the newest packet held as a baseline.
On the server, keep the newest ack you have seen (acks can arrive out of order too).

### Reordered and repeated packets

Packets may arrive late, twice or out of order (any unreliable channel does this on Wi-Fi and
mobile links). Only a packet newer than `SnapshotDecoder::last_seq()` (serial-number order, so
sequences may wrap) moves `current()` forward. An older or repeated packet still decodes and is
stored as a baseline, because the server may already have encoded a delta against it, but it
never moves the shown world back: its `DecodedSnapshot` carries the packet's own `seq`, `tick`
and `baseline`, the unchanged `current()` as `snapshot`, and empty `added`, `updated` and `removed`
lists. Tell a late packet apart with `decoded.seq != decoder.last_seq()`.

A *full* packet more than `capacity()` sequences behind the newest one is taken as a restarted
stream (a new server process starts again at `Seq(1)`) and resets the decoder. When you know the
stream restarted (a reconnect), call `SnapshotDecoder::reset()` instead of relying on that.

### Re-basing after the encoder lost the client's baseline

When the client's acks stop reaching the server for longer than the ring (a stall, an uplink
outage), the encoder's acknowledged baseline falls out of its ring and it sends full packets. Under
a byte budget smaller than the world, such a full packet carries only the highest-priority
entities. The decoder never lets that wipe the world: a full packet cut short by the budget is
*merged* into the shown world, and entities it did not carry stay shown at their last known state.
`SnapshotDecoder::is_rebasing()` is true while the shown world holds such entities; once acks flow
again the stream re-sends every entity over a few packets (each acknowledged packet is progress)
and the first complete packet settles the world exactly. While re-basing, an entity the server
removed meanwhile stays shown until that complete packet.

### Memory: shared baselines

Every packet is a baseline, so both sides keep up to `capacity()` of them. Baselines share their
storage copy-on-write: a baseline is a root of shared leaves of about 16 entities, and a packet
that changes `k` entities copies the root's pointer list, at most `k` leaves and the `k` new
values. 64 baselines of a mostly static 4,000-entity world hold less than twice what one does, and
an encode allocates in proportion to the records it writes, not to the world (the
`snapshot_delta_alloc` tests count both).

## Byte budgets

`budget_bytes` is a hard cap: a returned packet never exceeds it. Entity records pack by
descending `EntitySnapshot::priority` (ties break on the lower id; removals pack first). What
does not fit is listed in `EncodeReport::omitted` and the packet is marked *incomplete*.

Both sides store every packet, complete or not, as a baseline holding exactly what it carried:
the acknowledged baseline with the included records applied, omitted entities left at their
baseline state (or absent, for an omitted add). An overflow therefore defers an update but never
corrupts reconstructed state, and acknowledging an incomplete packet is progress: the next delta
carries only what is still missing, so a world larger than one packet converges over a few
packets. Priorities are taken as given; if high-priority entities change every tick and fill
every packet, low-priority ones wait, so raise a waiting entity's priority over time (a priority
accumulator) when that matters. A budget too small for even an empty packet returns
`SnapshotError::BudgetTooSmall` and consumes no sequence number.

## Wire format and failure behaviour

```text
packet  := magic u8 | version u8 | total_len u32le | crc32 u32le | body
body    := schema_hash u32 | flags u8 | seq varint | tick varint
           | baseline varint (0 = full snapshot) | count varint | record*
record  := kind u2 | id varint | payload        (kind: 0 add, 1 update, 2 remove)
add     := every field, in schema order
update  := change mask (one bit per field) | the changed fields, in schema order
```

The body is bit-packed least-significant-bit first (the crate's `bits::BitWriter`); the outer
header is byte-aligned. `total_len` makes every truncation a `LengthMismatch`, and the IEEE
CRC-32 over the body catches corruption before any field is read. Decoding untrusted bytes never
panics: short, truncated, corrupt, schema-mismatched or baseline-less packets return a
`SnapshotError` and leave the decoder exactly as it was. Encoding validates the snapshot against
the schema and reports duplicates, type mismatches and out-of-range enum discriminants as errors.

Encoding is deterministic: the same schema, world, tick, ack and budget produce the same bytes on
every platform, and the order of entities in the input snapshot does not matter.

## Limits

- up to `MAX_FIELDS` (128) fields per schema, `MAX_ENTITIES` (65 536) entities per snapshot;
- entity ids are the crate's `NetId`, ticks are `NetTick`; sequence numbers are this module's `Seq`;
- this layer does no transport, ordering, interpolation or relevance filtering — feed it packets
  from whatever channel you like, one packet per `decode` call.
