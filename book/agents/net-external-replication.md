# Net external replication (agents)

Replication of a game-owned simulation (no ECS world) to many clients: `regia_net::external_replication`
(spec 120 § 8.6, ADR-8186). The game owns the `ExternalReplication` value, so the engine cannot find it on its own: the
game publishes its stats under a label into the `NetExternalStats` resource, and agents read them with
`net.external_stats` (ADR-8892). Failures of the replication itself are `ExternalError::code()` values.

## Publishing (the game)

```text
// Once a second is plenty: stats() allocates one row per client.
if tick.0 % 60 == 0 {
    let mut published = world.resource_mut::<NetExternalStats>();
    replication.publish_stats("world", tick, &mut published);
}
```

Insert the resource once (`app.init_resource::<NetExternalStats>()`); without the `ecs` feature `NetExternalStats` is a
plain value your own agent reply can hold. At most 16 labels are kept (`NetExternalStats::MAX_LABELS`): a new label
past that is refused (`publish` returns `false`), an existing one always refreshes; `remove(label)` forgets one.

## `net.external_stats`

Params: `{}` for every published label, or `{ "replication": "<label>" }` for one. Read-only, no socket.

Result: `{ replications: [...] }`, one entry per label, sorted by label. Each entry is `ExternalStats` exactly as serde
writes it (`entities`, `band_radii_m`, `clients`; the table below) plus:

| Key | Meaning |
|---|---|
| `label` | The label the game published under. |
| `tick` | The game tick the snapshot was taken at (the stats are as of that tick, not live). |
| `publishes` | How many times the label was published; a value that stops growing means the game stopped publishing. |
| `totals` | Sums over `clients[]`: `clients` (the count), `packets`, `bytes`, `full_packets`, `records`, `omitted`, `tier_held`, `tier_sent` (`[near, mid, far]`), `budget_skips`, `acks`, `stale_acks`, `relevant`. |

Without the resource, or before the first publish, the list is empty.

```json
{"jsonrpc":"2.0","id":1,"method":"net.external_stats","params":{"replication":"world"}}
```

## What the stats say

`ExternalStats`:

| Key | Meaning |
|---|---|
| `entities` | Entities indexed (keys the game upserted and did not remove). |
| `band_radii_m` | The shared interest grid's band radii, one per spatial class radius. |
| `clients[]` | One row per client, by id. |
| `clients[].peer` | The client's `PeerId`. |
| `clients[].rate` | The rate the budget uses, bytes per second: `min(project, declared, allowance)`, not below 4096. |
| `clients[].ack` | The newest acknowledged baseline sequence (`null` before the first ack). |
| `clients[].counters.packets`, `bytes` | Packets and bytes encoded for the client. |
| `clients[].counters.full_packets` | Packets with no usable baseline (first packet, or acks lost longer than the ring). |
| `clients[].counters.records` | Records packed (adds, updates, removes). |
| `clients[].counters.omitted` | Records the byte budget left for a later send (never lost; their priority grows). |
| `clients[].counters.tier_held` | Updates the distance rate tiers held back. |
| `clients[].counters.tier_sent` | Records sent by tier, `[near, mid, far]`. |
| `clients[].counters.budget_skips` | Sends skipped because the budget could not hold a packet header (the budget carries). |
| `clients[].counters.acks`, `stale_acks` | Acks whose baseline was taken, and acks whose baseline was older or no longer held. |
| `clients[].counters.relevant` | Entities the client holds after the last send. |
| `clients[].counters.last_budget`, `last_bytes` | The last send's byte budget and packet size. |

Reading them: `omitted` growing while `last_bytes` sits at `last_budget` means the client's budget is the limit (raise
the rate, lower `max_interval_ticks`, or add tiers); `stale_acks` close to `acks` is normal under reordering; a
`full_packets` count that keeps growing means acks are not arriving (check the uplink and that the client sends
`ExternalAck::to_bytes()`); `budget_skips` above zero means the rate is too low for even a header per send.

The client side has `ExternalInterpolator::counters()`: `packets`, `decode_errors`, `late` (older than the newest:
kept as a baseline, no samples), `samples`, `snaps` (forward playback jumps).

## Error codes

`net.external_stats`:

| Code | When |
|---|---|
| `external_bad_params` | The params have a key other than `replication`, or `replication` is not a non-empty string. (Params that are not an object are refused by the dispatcher with `F0002`.) |
| `external_unknown` | No snapshot is published under that label (call with `{}` to list the labels). |

`ExternalError::code()` (the replication's own calls):

| Code | When |
|---|---|
| `invalid_config` | A setting is out of range (`ExternalConfig::validate`; the error names the field). |
| `unknown_client` | A call named a client that was never added (or was removed). |
| `duplicate_client` | `add_client` for an id that exists. |
| `unknown_class` | `upsert` named a class index the config does not have. |
| `bad_position` | `upsert` with a non-finite position. |
| `encode_failed` | The codec refused a client's packet: a record does not match the schema. |
| `decode_failed` | The client could not decode a packet; it is dropped and the stream recovers. |

## Checks an agent can run

- Every client converges: compare `latest(key)` on each client with the server's record for `relevant(peer)`.
- Budget held: per client, `bytes` over a run stays at or under `sends × rate / send_hz`.
- Tier rates: a far entity gets at most one record per `1 / far_hz` seconds (`tier_sent[2]` over time).

`net.external_stats` is checked by the `regia_agent` lib test
`net_external_stats::tests::net_external_stats_matches_stats_after_a_seeded_run` (the method's numbers equal
`stats()` after a seeded lossy run) and the `regia_net` lib test
`external_replication::tests::net_external_stats_holds_what_stats_returns_after_a_seeded_run`.

The acceptance test is `regia_net` lib test
`external_replication::tests::two_thousand_entities_sixteen_clients_on_bad_wifi_converge_within_budget_and_tiers`
(run it with `--no-default-features`: no ECS crate in the build).
