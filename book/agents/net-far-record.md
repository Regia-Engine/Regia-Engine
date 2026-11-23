# Net far record (agents)

Spec 120 § 8.3, ADR-10124 (amends ADR-10052 (1)) and ADR-10199 (the settle rule); transform record revision 4, wire 6.

A profile with `rate_tiers` sends a far copy (beyond `mid_m`, 600 m by default) at `far_hz` (2 Hz by default). On the
unreliable state channel such an update travels as a **far record**: only a coarse position, instead of the position,
rotation and velocity fields of an exact update.

| | Exact update | Far record |
|---|---|---|
| State record kind | 2 | 3 |
| Position | whole steps (1 mm by default), delta against the acknowledged baseline | the offset in the baseline's cell rounded to 32 steps on x and z and 1,024 steps on y: within 1.6 cm and 0.512 m of the server's position at the 1 mm step |
| Rotation, scale | sent when changed | not sent: the copy keeps what it shows |
| Velocity | sent when changed | not sent and cleared from the record, and the copy's `NetVelocity` is removed (ADR-10215): an interpolated proxy infers motion from the positions |
| Components | entries | the same entries |
| Size of the transform part | 6 bits to 20 bytes | 4 to 41 bits |

Rules an agent can rely on:

- Only far-tier updates on the acknowledged state channel (`peers[].wire` = `acked_deltas`) take the far form, and only
  when it is at least a byte shorter than the exact record (any shorter once the copy is held in its far form): a
  walker on a straight line, whose exact delta is a few predicted bits, stays exact. An owned or forced (`max_interval_ticks`) record, a teleport, a resend, a
  parent change, an entity with `extrapolation_gate_m`, a position off the grid (a child, a profile without a step)
  and a move into another position cell (1,048.576 m at the 1 mm step) send the exact record.
- A far copy whose coarse position did not change sends nothing (a turn in place is not sent).
- **A far copy settles** (ADR-10199): once its entity has not changed for `far_settle_seconds` (2 s), the copy gets
  one exact record, still in the far tier, and shows the server's exact position and rotation. After that a far copy
  at rest sends nothing and costs the server nothing.
- When a copy leaves the far tier before it settled, its next record carries the exact transform, even if the entity
  did not change since. Until one of the two the copy shows the coarse position and the rotation it last had.
- A far record is never the baseline of an exact transform delta, so loss, duplicates and reordering cannot leave a
  copy with a wrong transform. `record_errors` on the client stays 0.
- **A far copy has no `NetVelocity`** (ADR-10215): a proxy shown from a far record, or interpolating toward or from
  one, has the component removed, so gameplay and animation never read the velocity of an older exact record. The
  next exact record (tier exit or settle) writes the current velocity again. A copy the client owns or predicts keeps
  its own velocity.

## Reading it

| Method | Field | Meaning |
|---|---|---|
| `net.state_channel` | `far_records` | `true` while far-tier updates may travel as far records (the default); `false` after the host called `ServerReplication::set_far_records(false)` |
| `net.state_channel` | `peers[].far_records` | far records sent to this connection |
| `net.state_channel` | `far_settle_seconds` | 2: how long an entity stands before its far copies get the exact transform |
| `net.state_channel` | `transform_record.revision` | 4 from this build on |
| `net.state_channel` | `transform_record.far_record_bits`, `far_xz_shift`, `far_y_shift` | 40, 5, 10: the absolute far position is `x u15 \| z u15 \| y u10`, and a coarse step is `2^shift` position steps |
| `net.relevance` | `peers[].tier_sent`, `tier_held` | records per distance tier at the last send, and updates the tiers held back |

A check for an agent: with a tiered profile and a moving crowd beyond `mid_m`, `peers[].far_records` grows at about
`far_hz` per far copy and stays close to `records_sent`'s growth; when the viewpoint moves next to the crowd,
`far_records` stops growing and every copy's transform matches the server's within half a position step. The same
holds without moving the viewpoint once the crowd has stood for `far_settle_seconds` plus a far interval and the
round trip: each copy got one more record (`peers[].records_sent` grew by the number of copies) and `far_records`
did not grow.

## Turning it off

`ServerReplication::set_far_records(false)` (host code; it is not a `.netprofile.ron` field) keeps exact records in
the far tier, as before revision 4: use it when far copies need their rotation or sub-metre height. Copies already
held in their far form get their exact state with their next record.

## Errors

No new error codes. A peer built with another transform record revision is refused at the handshake with
`protocol_mismatch`; a dev build's detail names `regia_net::replication::transform_record (different)`.

## Numbers

`docs/perf/net-far-record.md`: 40 clients on a 128-walker crowd, with and without the far record.
