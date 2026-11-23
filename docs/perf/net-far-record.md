# Net far record: bandwidth of the far tier (spec 120 § 8.3, ADR-10124)

Source: `crates/regia_net/tests/far_tier_record.rs` `bandwidth_of_the_far_tier_with_and_without_the_far_record`
(`cargo test -p regia_net --test far_tier_record -- --nocapture`). Deterministic: a loopback hub at 60 Hz with fixed
steps and seeds, so the numbers do not depend on the host (measured on the test machine, lane head of 2026-11-20).

Workload: 128 walkers (1.2-1.6 m/s) in a 90 m square 800 m from the origin, 40 clients, a 100 ms round trip, the
default rate tiers (near below 150 m every send, mid below 600 m every third send, far at 2 Hz), a send every 2nd
tick, a budget that never binds; 180 warm-up ticks, then 600 measured ticks (10 s). Crowds: `straight` (constant
heading and pace), `turning` (a new heading of up to +-1 rad and a new pace every 1-3 s: the quantized-transform
crowd), `milling` (a new heading every 10-40 ticks and a slow bob on y: nothing the baseline's velocity predicts).
Viewers: `all far` (every viewer near the origin, so every update is a far-tier update) and `mixed` (viewers on a
line through the crowd every 40 m, so each sees near, mid and far walkers; about a quarter of the walker-viewer
pairs are far). "Off" is `ServerReplication::set_far_records(false)`: the far tier sends its exact delta at the far
rate, the behavior before transform record revision 4.

Columns: state-frame bytes per second and client; the same less the empty frames every client gets anyway (one per
send tick, 176 B/s); update records per second and client; far records in the window, all clients; record bytes per
record (framing included: id, length, kind, tick distances).

| Crowd | Viewers | Far record | State B/s per client | Record B/s per client | Records/s per client | Far records | B per record |
|---|---|---|---|---|---|---|---|
| straight | all far | off | 1,827 | 1,651 | 256.0 | 0 | 6.45 |
| straight | all far | on | 1,816 | 1,641 | 256.0 | 4,720 | 6.41 |
| straight | mixed | off | 9,559 | 9,384 | 1,496.4 | 0 | 6.27 |
| straight | mixed | on | 9,557 | 9,381 | 1,496.4 | 1,180 | 6.27 |
| turning | all far | off | 2,246 | 2,071 | 256.0 | 0 | 8.09 |
| turning | all far | on | 1,742 | 1,566 | 256.0 | 102,400 | 6.12 |
| turning | mixed | off | 10,352 | 10,176 | 1,497.2 | 0 | 6.80 |
| turning | mixed | on | 10,227 | 10,052 | 1,497.2 | 25,617 | 6.71 |
| milling | all far | off | 4,026 | 3,850 | 256.0 | 0 | 15.04 |
| milling | all far | on | 1,757 | 1,582 | 256.0 | 102,400 | 6.18 |
| milling | mixed | off | 16,555 | 16,379 | 1,500.1 | 0 | 10.92 |
| milling | mixed | on | 15,988 | 15,812 | 1,500.1 | 25,644 | 10.54 |

Reading it:

- **The far tier's record bytes fall 24 % on the turning crowd and 59 % on the milling crowd** (2,071 to 1,566 and
  3,850 to 1,582 B/s per client for 128 far walkers). A far record with its framing is 6.1 to 6.2 bytes whatever the
  walker does: the coarse residual is 24 bits for a walker half a second on, and rotation and velocity changes, which
  are what makes the exact records of the milling crowd 15 bytes on average, are not sent.
- **A crowd on straight lines gains nothing** (0.6 %): its exact delta is already a velocity-predicted residual of a
  few bits (ADR-8522), as short as the far record, so those walkers stay exact (the far form must be a byte shorter
  to be taken). 4.6 % of the records took the far form, where the prediction missed by more than a byte.
- **Mixed viewers: 0 to 3.5 % of all state bytes.** The far tier is 2 Hz against 30 Hz near and 10 Hz mid, so it is
  a small share of a client's traffic unless most of what it sees is far.
- The far record never cost bytes on any row (the test's bar), and on the two crowds the velocity does not predict
  the far tier's record bytes must stay at or below 90 % of the exact records' (measured: 76 % and 41 %).
- What the bytes buy is paid in precision: a far copy is within 1.6 cm (x, z) and 0.512 m (y) of the server's
  position and keeps its last rotation (`a_far_crowd_travels_as_far_records_and_comes_back_exact`); the exact
  transform returns with one absolute record (about 22 bytes) when the copy leaves the far tier.
- ADR-10052 (1) named 7.25 B per moving update as the far tier's cost to beat. That figure is the 256-walker crowd of
  `quantized_transform.rs` on one client with world relevance; the comparable rows here are `turning`: 8.09 B per
  far-tier record without the far record, 6.12 with it.

## The server's walk at rest (ADR-10199)

A copy held in its far form is "behind" until its exact state is acknowledged, so the gather walks it, the priority
pass reads its slot and the packer encodes it on every send tick. The settle rule ends that two seconds after the
entity's last change. Test `a_far_crowd_at_rest_settles_exact_and_leaves_the_walk` (64 turning walkers, one far
viewer, 100 ms round trip; counts of one gather, deterministic):

| Far records | A second after the crowd stops: copies walked / priority slots read | After the settle time | Records to settle | Then |
|---|---|---|---|---|
| off (exact records) | 0 / 0 | 0 / 0 | 0 | silent |
| on | 64 / 64, on every send tick | 0 / 0 | 64 (one exact record per copy, about 22 bytes) | silent, also when the viewer walks up |

Not measured here: the `regia_net_soak` profiles. That harness runs the ECS-free sample game on the raw transport,
not replication, so no soak scenario has far-tier entities; the loss, reorder and duplicate behavior of the far record
is tested on the hub (`far_and_exact_records_mix_soundly_under_loss_reorder_and_duplication`: 20 % loss, 25 %
reorder, 10 % duplicates, four seeds; `far_and_exact_records_mix_soundly_over_a_seed_sweep`: 64 seeds in the suite,
each with its own link of 5 to 35 % loss, up to 6 ticks of delay, up to 30 % duplicates and up to 50 % reordering;
3,000 seeds ran clean once, 1,688,992 far records).

Net evidence ledger row (for `docs/releases/net-evidence-ledger.md`; the lead appends it):

| Tag / line | Head | Run | Crowd, viewers | Far record off: record B/s per client, B/record | Far record on: record B/s per client, B/record |
|---|---|---|---|---|---|
| lane v07-wire6-far-tier-record | see the lane report | hub, 40 clients, 128 walkers, 10 s | turning, all far | 2,071, 8.09 | 1,566, 6.12 |
| lane v07-wire6-far-tier-record | see the lane report | hub, 40 clients, 128 walkers, 10 s | milling, all far | 3,850, 15.04 | 1,582, 6.18 |
| lane v07-wire6-far-tier-record | see the lane report | hub, 40 clients, 128 walkers, 10 s | straight, all far | 1,651, 6.45 | 1,641, 6.41 |
