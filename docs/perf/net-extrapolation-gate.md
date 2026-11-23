# Net extrapolation gate: soak row (spec 120 § 8.2, ADR-8522)

Source: `crates/regia_net/tests/extrapolation_gate.rs` `soak_row_bytes_and_shown_error_on_impaired_links`
(`cargo test -p regia_net --test extrapolation_gate -- --nocapture`). Deterministic: a loopback hub at 60 Hz with fixed
steps and seeds, so the numbers do not depend on the host (measured on the build server, lane head of 2026-11-18).

Workload: 128 walkers (the quantized-transform crowd: 1.2-1.6 m/s, a new heading of up to +-1 rad every 1-3 s), one
client, the default profile with world relevance and the default budget (49,152 B/s, a send every 2nd tick), 120 warm-up
ticks then 600 measured ticks (10 s). The links are the soak's profiles as the hub models them (one tick is 16.7 ms; the
hub adds a tick to every delivery and reorders by 1-3 ticks): `good-wifi` (4 ms + 0-4 ms jitter, 0.2 % loss, 0.1 %
reorder, 0.05 % duplicates), `bad-wifi` (8 ms + 0-30 ms jitter, about 3 % loss with bursts, 1-2 % reorder, 0.5 %
duplicates) and the conditioner's `wan_stress` (80 ms + 0-20 ms, 8 % loss, 2 % duplicates, 15 % reorder). The
`regia_net_soak` harness itself runs the ECS-free sample game, not replication, so the row runs on the hub.

Columns: state-frame bytes over update records (bytes per moving entity update; every walker moves every tick), state
bytes per second, update records per second, and the shown error: the distance between a copy's `Transform` and the
server's position at the tick the client estimates it is showing (`ClientStateChannel::server_tick_estimate`), sampled
every tick for every copy. The copies have no interpolation clock: with the gate off a copy shows its newest record,
with the gate on it extrapolates the record (`NetSteady`).

| Link | Gate | B per moving entity update | kB/s | Updates/s | Worst shown m | Mean shown m |
|---|---|---|---|---|---|---|
| good-wifi | off | 5.70 | 21.8 | 3,827 | 0.080 | 0.0120 |
| good-wifi | 2 cm | 12.24 | 1.3 | 108 | 0.116 | 0.0082 |
| bad-wifi | off | 6.12 | 23.4 | 3,827 | 0.080 | 0.0130 |
| bad-wifi | 2 cm | 12.24 | 1.3 | 109 | 0.137 | 0.0082 |
| wan_stress | off | 7.03 | 25.0 | 3,558 | 0.186 | 0.0205 |
| wan_stress | 2 cm | 12.44 | 1.6 | 127 | 0.432 | 0.0094 |

Reading it:

- **Gate off (the default):** the velocity-predicted residual alone takes the quantized-transform crowd test from 9.03 to
  7.25 B per moving entity update (`quantized_transform.rs`, 256 walkers, 100 ms round trip; 163 updates per send tick
  on the default budget, was 142). Positions still land within half a step (0.50 mm at 1 mm).
- **Gate on (2 cm):** 15-17x fewer bytes per second and 28-35x fewer records; the records that are sent are the
  expensive ones (a turn), so bytes per update roughly double. The mean shown error falls (the copy is extrapolated
  instead of shown a tick or two old); the worst rises: after a turn the client shows the old heading until the server
  sees the error past the gate at a send tick and the record lands (send interval plus one-way delay), longer when that
  record is lost (until the link resolves it, about a round trip and a half).
- Straight walkers (`straight_walkers_cost_a_fraction_with_the_gate_and_stay_within_it`, 64 walkers, 100 ms round
  trip): 1,739 bytes in 4 s with the gate against 47,890 without (27x), worst shown error 2.07 cm (the gate plus the
  1/32 m/s velocity grid's drift while a record travels), mean 1.01 cm.
- Every gated crowd, stopped, lands within the gate (+1 mm) of the server on every link, with loss, reorder and
  duplicates (three seeds).

Net evidence ledger row (for `docs/releases/net-evidence-ledger.md` once net-scale-soak lands; the lead appends it):

| Tag / line | Head | Run | Link | Gate off: B/update, kB/s, worst/mean shown m | Gate 2 cm: B/update, kB/s, worst/mean shown m |
|---|---|---|---|---|---|
| lane net-extrapolation-gate | see the lane report | hub, 128 walkers, 10 s | good-wifi | 5.70, 21.8, 0.080/0.0120 | 12.24, 1.3, 0.116/0.0082 |
| lane net-extrapolation-gate | see the lane report | hub, 128 walkers, 10 s | bad-wifi | 6.12, 23.4, 0.080/0.0130 | 12.24, 1.3, 0.137/0.0082 |
| lane net-extrapolation-gate | see the lane report | hub, 128 walkers, 10 s | wan_stress | 7.03, 25.0, 0.186/0.0205 | 12.44, 1.6, 0.432/0.0094 |
