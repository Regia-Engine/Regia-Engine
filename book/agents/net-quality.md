# Net quality (agents)

What a `regia_net` endpoint knows about each link, graded for a HUD's connection icon and for the game's own decisions
(spec 120 § 13, ADR-8182): `net.quality`, the `grade` of every [`net.peers`](net.md) row, the client's `quality` in
`net.status`, and the `regia_net::stats` operator rows of a dedicated server.

## `net.quality`

| Param | Type | Meaning |
|---|---|---|
| `endpoint` | string, optional | Endpoint label (`net.status` lists them); all endpoints when omitted. |
| `peer` | integer, optional | Keep only this server peer's row. |

```json
{ "endpoint": "server" }
→ { "limits": { "good": { "rtt_p95_ms": 150.0, "loss": 0.02, "inputs_missing": 0.005 },
                "fair": { "rtt_p95_ms": 300.0, "loss": 0.05, "inputs_missing": 0.02 } },
    "endpoints": [{ "label": "server", "role": "server", "grade": null, "interrupted": false, "quality": null,
      "peers": [{ "id": 1, "name": "ada", "grade": "fair", "interrupted": false,
        "quality": { "rtt_ms": 41.2, "rtt_p50_ms": 39.0, "rtt_p95_ms": 74.0, "rttvar_ms": 9.8, "jitter_ms": 10.4,
                     "loss_out": 0.031, "loss_in": 0.0, "loss_recv": 0.029, "queue_delay_ms": 0.0,
                     "send_allowance_bps": 61440, "sent_bps": 4200, "input_margin_p5": 2,
                     "inputs_late": 0.0, "inputs_missing": 0.0, "grade": "fair", "interrupted": false } }] }] }
```

A client endpoint answers its link to the server in `grade`, `interrupted` and `quality` (null before it connects) with
an empty `peers` list.

### The signals

| Field | Meaning |
|---|---|
| `rtt_ms`, `rttvar_ms` | Smoothed round-trip time and its variation (RFC 6298 `SRTT`, `RTTVAR`). |
| `rtt_p50_ms`, `rtt_p95_ms` | Median and 95th percentile of the raw round-trip samples of the last 10 s. |
| `jitter_ms` | One-way interarrival jitter of what the peer sends (RFC 3550 § 6.4.1), from the server's stamp in each tick sync (client) and the client's clock record every 250 ms (server). |
| `loss_out` | Loss on what this end sent, from its acknowledgments, last 10 s. |
| `loss_in` | Loss on what this end sent as the peer measured it: on a client the upstream loss the server reports in its input feedback. `0` on a server (clients send no loss report). |
| `loss_recv` | Loss on what this end received, from packet numbers, last 10 s: the downstream for a client, the upstream for the server (the value it reports to the client). |
| `input_margin_p5` | The server-measured 5th percentile input arrival margin, ticks. |
| `inputs_late`, `inputs_missing` | Fractions of simulated ticks over the last 10 s whose input arrived late or not at all (server-measured; a client reads them from the feedback). |
| `queue_delay_ms`, `send_allowance_bps` | The rate controller's queueing delay over the minimum RTT in its latest round and the rate the game's own traffic may use now (spec 120 § 4.2; `send_allowance_bps` is 4294967295 with the controller off). |
| `sent_bps` | What this end sent over the last second, bytes per second. |

Loss fractions read `0` until the window holds 67 resolved packets (so one lost packet never reads as a loss rate past the `good` band); input fractions until it holds 30 ticks.

### The grade

| Grade | When |
|---|---|
| `good` | RTT p95 under 150 ms, loss (the worst of `loss_out`, `loss_in`, `loss_recv`) under 2 %, missing inputs under 0.5 %. |
| `fair` | RTT p95 under 300 ms, loss under 5 %, missing inputs under 2 %. |
| `poor` | Over a `fair` limit. |
| `interrupted` | The peer is silent past `interrupt_ms` (spec 120 § 10.2). |

Each boundary is a Schmitt trigger: a worse grade starts when a signal passes its limit and shows after 500 ms in which
the signals stayed past 75 % of it (the grade shown is the strict one then); a better one shows after 2 s with every
signal under 75 % of the better grade's limits, so a signal sitting on a limit does not flicker the icon. A silent link
(nothing received since the last 250 ms sample) moves no grade: silence is `interrupted`'s to judge. `interrupted` shows at once, once per outage,
and the grade from before the outage comes back with the restore; the loss windows restart 1 s after it, so packets
lost inside the outage do not count against the link. The endpoints raise `ServerEvent::QualityChanged { peer, from,
to }` and `ClientEvent::QualityChanged { from, to }` on each change.

### Errors

| Code | When |
|---|---|
| `net_unknown_endpoint` | `endpoint` names no published endpoint. |
| `peer_unknown` | `peer` is not a peer of any listed endpoint. |

## Why does the game feel bad?

1. `net.quality` for the client: is the grade `fair` or `poor`, and which signal is over its limit?
2. `loss_in` high, `loss_recv` low: the upstream loses packets (the player's upload). The reverse: the downstream.
3. `rtt_p95_ms` high with `rtt_p50_ms` low: spikes (Wi-Fi or a busy link), not distance; `jitter_ms` says on which leg.
4. `inputs_missing` or `inputs_late` above zero with clean loss: the inputs arrive too late; see the input lead in
   [Net input](net-input.md).
5. `net.status` `totals.socket_drops` above zero: this machine's socket refused datagrams (a full send buffer); they
   are not counted in `datagrams_out`.

## Operator rows (`regia_net::stats`)

A dedicated server (`regia_net::server_host`) writes a tracing row to the target `regia_net::stats` every 10 s per peer
and a server row every 60 s; clients write none. Addresses read `redacted` unless the host sets
`StatsConfig::show_addresses`.

```text
peer=1 addr=redacted grade=fair rtt_p50_ms=39.0 rtt_p95_ms=74.0 jitter_ms=10.4 rttvar_ms=9.8 loss_out=0.0310 loss_in=0.0000 loss_recv=0.0290 margin_p5=2 inputs_late=0.0000 inputs_missing=0.0000 sent_bps=4200 recv_bps=1900
server ticks=1800 tick_step_ms_p50=0.410 tick_step_ms_p95=0.620 tick_step_ms_p99=0.900 loop_ms_p50=0.700 loop_ms_p95=1.100 loop_ms_p99=1.600 rss_bytes=48230400 peers_good=7 peers_fair=1 peers_poor=0 peers_interrupted=0 sent_bps=33600 recv_bps=15200
```

`rss_bytes` is `unknown` off Linux and Android. Filter them with `RUST_LOG=regia_net::stats=info`.
