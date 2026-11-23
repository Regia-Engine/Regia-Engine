# Resume times with the one-round-trip resume (wire 6)

Wire 6 lets a returning client come back in one round trip: its first datagram carries the resumption ticket, the
proof that it holds the link's secret and a new key share (spec 120 § 10.3, § 11.4; ADR-8858, ADR-8902, ADR-10088).
This page records how long a rejoin takes with it, per network profile of the net soak, against the full handshake of
three round trips.

## What is measured

The soak's scripted rejoin (`book/agents/net-soak.md`): every eighth client leaves at 30 % of the run and connects
again two seconds later, with the ticket and the credential its leave took. The time is from the instant the
rejoining client's link is up to the instant it is connected, the number the `reconnect_within_grace` bar reads
(grace 3,000 ms). Virtual time, in-process network, 60 s per run, the gate plan's replication rows (8 and 32 clients;
`cgnat` at 10; `mixed` also at 40; two listen rows), six seeds per row: 1592590337 (the gate seed), 11, 22, 33, 44, 55.
Times are quantized by the 16 ms harness step.

Each row runs twice per seed:

- **Plan as is**: the profile as the gate plan runs it. Only `cgnat` and `asym-bufferbloat` servers keep reconnect
  tickets there (`Profile::server_resume_grace_ms`), so every other row's rejoin is the full handshake.
- **Tickets kept**: the same run with the server keeping tickets for 60 s (`Profile::resume_grace_ms`), as a `NetLink`
  server and a `NetSession` host with a reconnect grace do. Every rejoin is then a one-round-trip resume.

Reproduce: `cargo test --release -p regia_net_soak --test resume_time_table -- --ignored --nocapture` (about 95 s on
8 threads; `RESUME_TABLE_SEEDS=1,2,3` chooses other seeds). Measured on the test machine, 2026-11-20, lane
v07-wire6-soak-bars.

## Table

Times in milliseconds, p50 / p99 / max over the scripted rejoins of all six seeds. "One-round-trip resumes" is the
server's `resumed_one_rtt` total over the six runs. "Involuntary ends" counts every connection end that was not a
goodbye, the timeouts of a profile's own scripted outages included (`flappy`, `long-outage`, `mixed`); with tickets
kept, a server-side timeout is a dormancy and is not an end.

| Row (profile/clients/mode) | Rejoins | Plan as is: p50 / p99 / max | One-round-trip resumes | Tickets kept: p50 / p99 / max | One-round-trip resumes | Involuntary ends (as is / kept) |
|---|---|---|---|---|---|---|
| clean/8/dedicated | 6 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 6 | 0 / 0 |
| good-wifi/8/dedicated | 6 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 6 | 0 / 0 |
| bad-wifi/8/dedicated | 6 | 208 / 416 / 416 | 0 | 64 / 320 / 320 | 6 | 0 / 0 |
| mobile-4g/8/dedicated | 6 | 384 / 400 / 400 | 0 | 128 / 128 / 128 | 6 | 0 / 0 |
| transatlantic/8/dedicated | 6 | 352 / 384 / 384 | 0 | 112 / 128 / 128 | 6 | 0 / 0 |
| congested/8/dedicated | 6 | 192 / 208 / 208 | 0 | 80 / 112 / 112 | 6 | 0 / 0 |
| flappy/8/dedicated | 6 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 6 | 30 / 12 |
| hostile/8/dedicated | 6 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 6 | 0 / 0 |
| high-rtt/8/dedicated | 6 | 2080 / 2112 / 2112 | 0 | 704 / 720 / 720 | 6 | 0 / 0 |
| rtt-step/8/dedicated | 6 | 192 / 192 / 192 | 0 | 64 / 64 / 64 | 6 | 0 / 0 |
| clock-drift/8/dedicated | 6 | 96 / 96 / 96 | 0 | 32 / 32 / 32 | 6 | 0 / 0 |
| nat-rebind/8/dedicated | 6 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 6 | 0 / 0 |
| cgnat/10/dedicated | 6 | 48 / 48 / 48 | 6 | 48 / 48 / 48 | 6 | 0 / 0 |
| wifi-to-cell/8/dedicated | 6 | 192 / 192 / 192 | 0 | 48 / 64 / 64 | 6 | 0 / 0 |
| long-outage/8/dedicated | 6 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 6 | 60 / 30 |
| bufferbloat/8/dedicated | 6 | 192 / 192 / 192 | 0 | 64 / 64 / 64 | 6 | 0 / 0 |
| downstream-loss/8/dedicated | 6 | 208 / 1008 / 1008 | 0 | 64 / 608 / 608 | 6 | 0 / 0 |
| asym-bufferbloat/8/dedicated | 6 | 1040 / 1280 / 1280 | 6 | 1040 / 1280 / 1280 | 6 | 0 / 0 |
| listen-uplink/8/dedicated | 6 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 6 | 0 / 0 |
| lossy-fat-pipe/8/dedicated | 6 | 192 / 192 / 192 | 0 | 64 / 64 / 64 | 6 | 0 / 0 |
| rtt-extreme/8/dedicated | 6 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 6 | 0 / 0 |
| mtu-1280/8/dedicated | 6 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 6 | 0 / 0 |
| mtu-blackhole/8/dedicated | 6 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 6 | 0 / 0 |
| flood/8/dedicated | 6 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 6 | 0 / 0 |
| forged-handshake/8/dedicated | 6 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 6 | 0 / 0 |
| mixed/8/dedicated | 6 | 208 / 416 / 416 | 0 | 64 / 320 / 320 | 6 | 12 / 6 |
| congested+bulk/8/dedicated | 6 | 192 / 208 / 208 | 0 | 80 / 112 / 112 | 6 | 0 / 0 |
| bad-wifi/32/dedicated | 24 | 208 / 992 / 992 | 0 | 64 / 320 / 320 | 24 | 1 / 0 |
| mobile-4g/32/dedicated | 24 | 384 / 1392 / 1392 | 0 | 112 / 656 / 656 | 24 | 0 / 0 |
| flappy/32/dedicated | 24 | 112 / 384 / 384 | 0 | 48 / 48 / 48 | 24 | 120 / 48 |
| hostile/32/dedicated | 24 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 24 | 0 / 0 |
| high-rtt/32/dedicated | 24 | 2080 / 2352 / 2352 | 0 | 704 / 720 / 720 | 24 | 0 / 0 |
| rtt-step/32/dedicated | 24 | 192 / 192 / 192 | 0 | 64 / 64 / 64 | 24 | 0 / 0 |
| clock-drift/32/dedicated | 24 | 96 / 96 / 96 | 0 | 32 / 32 / 32 | 24 | 0 / 0 |
| nat-rebind/32/dedicated | 24 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 24 | 0 / 0 |
| cgnat/32/dedicated | 24 | 48 / 48 / 48 | 24 | 48 / 48 / 48 | 24 | 0 / 0 |
| wifi-to-cell/32/dedicated | 24 | 192 / 448 / 448 | 0 | 64 / 64 / 64 | 24 | 0 / 0 |
| long-outage/32/dedicated | 24 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 24 | 252 / 126 |
| bufferbloat/32/dedicated | 24 | 192 / 192 / 192 | 0 | 64 / 64 / 64 | 24 | 0 / 0 |
| downstream-loss/32/dedicated | 24 | 208 / 1008 / 1008 | 0 | 64 / 336 / 336 | 24 | 0 / 0 |
| asym-bufferbloat/32/dedicated | 24 | 1040 / 1280 / 1280 | 24 | 1040 / 1280 / 1280 | 24 | 0 / 0 |
| listen-uplink/32/dedicated | 24 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 24 | 0 / 0 |
| lossy-fat-pipe/32/dedicated | 24 | 192 / 464 / 464 | 0 | 64 / 320 / 320 | 24 | 0 / 0 |
| rtt-extreme/32/dedicated | 24 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 24 | 0 / 0 |
| mtu-1280/32/dedicated | 24 | 112 / 384 / 384 | 0 | 48 / 48 / 48 | 24 | 0 / 0 |
| mtu-blackhole/32/dedicated | 24 | 112 / 384 / 384 | 0 | 48 / 48 / 48 | 24 | 0 / 0 |
| flood/32/dedicated | 24 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 24 | 0 / 0 |
| forged-handshake/32/dedicated | 24 | 112 / 352 / 352 | 0 | 48 / 48 / 48 | 24 | 0 / 0 |
| mixed/32/dedicated | 24 | 192 / 992 / 992 | 0 | 64 / 320 / 320 | 24 | 25 / 12 |
| mixed/40/dedicated | 30 | 208 / 1104 / 1104 | 0 | 64 / 880 / 880 | 30 | 24 / 12 |
| good-wifi/8/listen | 6 | 112 / 112 / 112 | 0 | 48 / 48 / 48 | 6 | 0 / 0 |
| congested/8/listen | 6 | 192 / 240 / 240 | 0 | 112 / 112 / 112 | 6 | 0 / 0 |

## Reading it

- **Every rejoin with a ticket is one round trip.** In all 53 rows the "tickets kept" run counts as many one-round-trip
  resumes as rejoins; none fell back to the full handshake.
- **About a third of the time on a clean path.** `good-wifi` goes from 112 ms to 48 ms (ADR-8733 estimated about
  37 ms; the harness step is 16 ms), `transatlantic` from 352 to 112 ms, `mobile-4g` from 384 to 128 ms at 8 clients,
  `high-rtt` from 2,080 to 704 ms.
- **`asym-bufferbloat` is inside the grace.** Its downstream is a 1 s queue that cross traffic keeps full, so one
  round trip is about 1,040 ms: p50 1,040 ms, p99 1,280 ms at 8 and at 32 clients (ADR-8733 estimated 1,035 to
  1,125 ms). At v0.6.0 the same rejoin took the three round trips of the full handshake: 3,104 to 3,376 ms, past the
  3,000 ms grace, which is what kept the two `asym-bufferbloat` `reconnect_within_grace` bars of the gate plan red.
- **Loss stretches the tail, less than before.** Where the first flight or its answer can be lost the client sends
  the flight again: `bad-wifi` p99 320 ms (416 to 992 ms with the full handshake), `downstream-loss` 336 to 608 ms
  (1,008 ms), `mobile-4g` at 32 clients 656 ms (1,392 ms), `mixed` at 40 clients 880 ms (1,104 ms).

## The gate plan, before and after

`cargo xtask net-soak --plan gate --release` (61 runs), base = b3607bd51 (lane v07-wire6-resume-server), after = this
lane; both on the test machine, each tree in its own target directory.

| Seed | Before | After |
|---|---|---|
| 1592590337 (the gate seed) | FAIL: 2 red gating bars | PASS: 0 red gating bars |
| 20261120 | FAIL: 2 red gating bars | PASS: 0 red gating bars |

The two red bars before are `asym-bufferbloat` `reconnect_within_grace` at 8 and at 32 clients:

| Row | Bar | Before, seed 1592590337 / 20261120 | After, both seeds |
|---|---|---|---|
| asym-bufferbloat/8/dedicated | `reconnect_within_grace` (3,000 ms) | 3,104 ms / 3,104 ms | 1,040 ms |
| asym-bufferbloat/32/dedicated | `reconnect_within_grace` (3,000 ms) | 3,120 ms / 3,376 ms | 1,040 ms |
| cgnat/10/dedicated | `reconnect_within_grace` | 112 ms / 112 ms | 48 ms |
| cgnat/32/dedicated | `reconnect_within_grace` | 112 ms / 112 ms | 48 ms |

`cgnat` was green before; its server kept tickets already, and its rejoin now presents the credential.

## Known limit

Behind `asym-bufferbloat`'s full queue a reliable message can still stall, rarely: of 72 rows (36 seeds, 8 and 32
clients) one, 32 clients at seed 2213, loses a client to `reassembly_stalled` 23 s after its resume; the rejoins
themselves are one round trip in every row. The queue admits a full-size packet 48 % of the time (96 % for a packet
under 600 bytes) and the client's own snapshots take the room a full-size repair needs. ADR-10088 has the measurement
and the fixes that remain.
