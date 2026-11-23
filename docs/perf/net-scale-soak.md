# Net scale soak: server numbers per seat count (N215, ADR-8454)

What a dedicated and a listen server cost at the player counts spec 120 § 1.1 promises, recorded the way DO27 recorded its
40-seat server (CPU, memory, Mbit/s per seat), from `cargo xtask net-soak --plan scale` (the net soak's sample game:
inputs every frame with 15-deep redundancy and 0.5-1 KiB command tails, reliable 0.5-4 KiB commands and chat, a 16 KiB
world blob on join, a snapshot of every entity to every client each tick sized by the rate controller, scripted leaves
and rejoins; every link encrypted). The `tick_p99` budgets are the `net_scale_tick_p99_*` rows of
[`budgets.ron`](budgets.ron): a quarter of the 30 Hz tick up to 40 clients, a third up to 64, half above.

How to read the columns:

- **Tick**: wall time of a server iteration that ran a tick (receive, events, the soak's own checks, the game tick with a
  snapshot per client, flush), µs. **Phases** are the p99 of each part.
- **CPU**: every server iteration's wall time over the run's length, as a share of one core at real time. The run is
  single-threaded in virtual time, so this is the server thread's own work.
- **Allocs/tick**: the server's heap allocations per tick (the impairment network's datagram copies excluded).
- **Up / seat**: the server's upstream with 28 bytes of IPv4 and UDP header per datagram, in total and per client.
- **RSS**: the whole test process (server, every client, the network), end of run / peak, MiB.
- Host: the build server (128 threads) **shared and loaded** (1-minute load 170-205 during the runs), `nice 5`,
  one run at a time. Wall-clock columns on a loaded host overstate a quiet server; rerun on a quiet host for a budget
  verdict.

## 2026-11-18, lane base (04d9ad838 + net-relevance-scale + net-v2-rate-controller-c), 180 virtual s per run, seed 0x5EED0001

Virtual wire (in-process queues):

| Run | Tick p50 / p99 / max µs (budget) | Phase p99 µs: receive / events / harness / tick / flush | CPU % of a core | Allocs/tick p50 / p99 (per client) | Up Mbit/s (kbit/s per seat) | Down kbit/s per seat | Client max down / up kbit/s (budget 1000 / 256) | Staleness p99 ticks | Involuntary ends | RSS MiB |
|---|---|---|---|---|---|---|---|---|---|---|
| mixed/40 | 2,499 / 5,877 / 12,999 (8,333) | 3,542 / 18 / 28 / 426 / 1,871 | 8.0 | 1,622 / 1,801 (40.4) | 13.2 (330) | 152 | 454 / 204 | 10 | 0 | 19 |
| mixed+hostile/40 | 2,360 / 6,736 / 14,193 (8,333) | 4,325 / 293 / 27 / 403 / 1,709 | 7.9 | 1,658 / 13,249 (55.0) | 13.6 (341) | 201 | 454 / 204 | 17 | 0 | 21 |
| mixed/64 | 5,429 / 10,146 / 31,558 (11,111) | 6,032 / 21 / 44 / 636 / 3,960 | 17.8 | 2,625 / 2,840 (40.8) | 21.7 (340) | 153 | 454 / 208 | 7 | 0 | 25 |
| mixed+hostile/64 | 4,843 / 9,625 / 26,846 (11,111) | 5,541 / 427 / 42 / 630 / 3,711 | 15.6 | 2,662 / 14,139 (49.7) | 22.1 (345) | 184 | 454 / 208 | 10 | 0 | 27 |
| mixed/128 | 17,162 / 30,608 / 150,379 (16,666) | 16,473 / 43 / 115 / 2,276 / 13,133 | 58.0 | 6,500 / 7,017 (50.2) | 59.3 (463) | 157 | 632 / 217 | 16 | 0 | 42 |
| mixed+hostile/128 | 18,154 / 31,059 / 176,004 (16,666) | 16,396 / 893 / 117 / 2,120 / 12,678 | 59.5 | 6,556 / 18,182 (55.0) | 59.9 (468) | 173 | 632 / 217 | 19 | 0 | 44 |
| listen-uplink/16 (listen) | 251 / 537 / 13,237 (8,333) | 319 / 7 / 2 / 56 / 208 | 0.8 | 496 / 722 (31.2) | 1.9 (117) | 154 | 210 / 196 | 2 | 0 | 40 |
| listen-uplink/32 (listen) | 888 / 2,290 / 11,106 (8,333) | 1,352 / 10 / 16 / 236 / 684 | 3.0 | 1,503 / 1,607 (46.7) | 14.2 (444) | 163 | 450 / 208 | 2 | 0 | 40 |

A first pass of the same plan an hour earlier (without phase timing) gave tick p99 4,861 / 9,022 / 24,465 µs at 40 / 64 /
128 and 6.7 / 14.8 / 45.2 % of a core: the host's load moves these numbers by 20-30 %.

DO27's own record at 40 seats (its `do27_server`, a different game): 37 % of one core, 11.1 Mbit/s up, sim p99 2-3.3 ms.
The sample game here sends more per seat (330 kbit/s against 277) at 8 % of a core.

**Where the time goes.** The game's tick (simulation and one snapshot per client) is under 2.3 ms at every count;
`receive` (opening, acknowledging and decoding what arrived) and `flush` (packing, sealing, sending) are the rest. Per
received datagram, `receive` costs about 26 µs at 40 clients, 34 at 64 and 46 at 128 on `mixed` (7-9 µs on the
good-wifi listen runs), so it grows with the peer count: `Session::peer_by_addr` (session.rs:354) scans every peer for
each sealed datagram, and the rebind path scans every connection by token (server.rs:1275). Lane N103
net-rebind-path-validation replaces both with O(1) maps; re-measure 128 on the line that carries it.

Verdicts: 40 and 64 within their tiers; 128 over (30.6 ms against 16.7 ms); every run 0 involuntary disconnects and
every client within its bandwidth budget; the hostile peer at 40/64/128 is not dropped at this base (lanes N103 and
N105). The 64 and 128 bars are owner question Q3.

Real UDP wire (`--wire udp`, same plan, same host, right after; the game numbers are identical to the virtual wire's,
the tick adds the system calls):

| Run | Tick p50 / p99 / max µs (budget) | Phase p99 µs: receive / events / harness / tick / flush | CPU % of a core | Involuntary ends | Datagrams through real sockets |
|---|---|---|---|---|---|
| mixed/40/udp | 3,657 / 6,530 / 10,682 (8,333) | 4,094 / 19 / 34 / 478 / 2,095 | 11.3 | 0 | 652,803 |
| mixed+hostile/40/udp | 3,590 / 10,036 / 19,968 (8,333) | 6,622 / 519 / 36 / 549 / 2,657 | 11.5 | 0 | 704,566 |
| mixed/64/udp | 6,448 / 12,369 / 23,654 (11,111) | 7,479 / 29 / 51 / 762 / 4,608 | 21.5 | 0 | 1,057,875 |
| mixed+hostile/64/udp | 6,086 / 13,387 / 35,621 (11,111) | 8,394 / 578 / 45 / 728 / 4,297 | 20.9 | 0 | 1,107,464 |
| mixed/128/udp | 15,706 / 27,410 / 236,046 (16,666) | 14,737 / 42 / 91 / 1,744 / 11,681 | 52.5 | 0 | 2,573,092 |
| mixed+hostile/128/udp | 17,992 / 29,942 / 230,093 (16,666) | 16,855 / 733 / 101 / 1,850 / 11,804 | 59.1 | 0 | 2,629,753 |
| listen-uplink/16/udp (listen) | 326 / 908 / 10,855 (8,333) | 541 / 10 / 7 / 86 / 291 | 1.1 | 0 | 188,220 |
| listen-uplink/32/udp (listen) | 2,372 / 4,720 / 19,478 (8,333) | 3,010 / 20 / 30 / 433 / 1,394 | 7.4 | 0 | 579,615 |

(This pass ran before the wire paced its buffers by bytes: in the `+hostile` runs the hostile peer's oversized
datagrams overflowed the kernel buffer, about 100 of them were dropped by the kernel, as a real server's buffer would.
The honest runs lost none.)

## 2026-11-18, minimum-spec Windows laptop (the min-spec Windows laptop MiniBook X, Intel N150, Windows 11), quiet, same build

The listen-server promise (16 players on the minimum-spec machine): `listen-uplink`, 180 virtual s, one run at a time,
the scale test binary cross-built for `x86_64-pc-windows-gnu`. Every scale test of `tests/scale.rs` also passes there,
the real UDP wire included.

| Run | Tick p50 / p99 / max µs (budget) | Phase p99 µs: receive / events / harness / tick / flush | CPU % of a core | Allocs/tick p50 / p99 | Up Mbit/s (kbit/s per seat) | Staleness p99 | Involuntary ends |
|---|---|---|---|---|---|---|---|
| listen-uplink/16 (listen) | 206 / 398 / 894 (8,333) | 238 / 1 / 1 / 45 / 128 | 0.7 | 496 / 722 | 1.9 (117) | 2 | 0 |
| listen-uplink/32 (listen) | 1,606 / 1,827 / 4,844 (8,333) | 884 / 5 / 7 / 280 / 774 | 4.8 | 1,503 / 1,607 | 14.2 (444) | 2 | 0 |
| listen-uplink/16/udp (listen) | 277 / 698 / 1,079 (8,333) | 452 / 3 / 2 / 82 / 211 | 1.2 | 496 / 722 | 1.9 (117) | 2 | 0 |
| listen-uplink/32/udp (listen) | 1,906 / 2,405 / 4,874 (8,333) | 1,480 / 6 / 7 / 278 / 784 | 5.8 | 1,503 / 1,607 | 14.2 (444) | 2 | 0 |
| mixed/40 (`scale-gate`, 60 s) | 820 / 2,154 / 2,285 (8,333) | 1,071 / 13 / 8 / 257 / 832 | 2.8 | 1,650 / 1,879 | 13.6 (341) | 65 | 0 |
| mixed+hostile/40 (`scale-gate`, 60 s) | 2,001 / 5,788 / 12,787 (8,333) | 4,390 / 312 / 9 / 323 / 930 | 7.5 | 1,698 / 13,699 | 13.9 (348) | 65 | 0 |

The quiet N150 runs the server tick faster than the loaded the build server (mixed/40 p99 2.2 ms against 5.9 ms), so the
the build server numbers above are upper bounds. A listen host at 16 players spends under 1 % of one N150 core on the netcode;
at 32 about 5 %. (The 60 s `mixed` runs' staleness p99 includes the flappy clients' outages in a short run.)

## 2026-11-20, v0.6.0 + the v0.6.1 net lanes, before and after ADR-10081 (lane v07-net-scale-128), quiet the test machine core

Host: the test machine (Ryzen 9 7950X3D, 32 threads), the run pinned to core 31 (`taskset -c 31`), 1-minute load 21-30
from other lanes' builds on the other cores; one run at a time, virtual wire, seed 0x5EED0001, 180 virtual s per run
(`REGIA_NET_SOAK_PLAN=scale` through the `scale` test binary, which counts allocations). "Before" is the lane's base
7e26d3280 (the v0.6.0 tag 02381fd90 plus v06-net-soak-followups, net-rpc-p95-up-leg, v06-net-review-fixes and
v06-security-gaps-fixes); "after" adds ADR-10081. Every other recorded field of the eight runs (joins, ends, losses,
staleness, bytes, every counter) is identical before and after: the change moves time and allocations only.

| Run | Before: tick p50 / p99 / max µs (budget) | After: tick p50 / p99 / max µs | Phase p99 µs before: receive / events / harness / tick / flush | Phase p99 µs after | CPU % of a core before / after | Allocs/tick p50 / p99 (per client) before | after | Up Mbit/s (kbit/s per seat) | Involuntary ends |
|---|---|---|---|---|---|---|---|---|---|
| mixed/40 | 523 / 1,084 / 1,492 (8,333) | 384 / 485 / 990 | 492 / 4 / 6 / 102 / 485 | 218 / 4 / 2 / 44 / 253 | 1.8 / 1.2 | 1,696 / 1,852 (42.2) | 914 / 1,023 (22.8) | 13.4 (335) | 0 |
| mixed+hostile/40 | 481 / 1,162 / 2,117 (8,333) | 309 / 807 / 2,317 | 722 / 61 / 5 / 89 / 380 | 542 / 54 / 2 / 43 / 278 | 1.7 / 1.0 | 1,699 / 11,490 (45.3) | 917 / 10,687 (25.9) | 13.4 (335) | 0 |
| mixed/64 | 1,327 / 2,224 / 4,327 (11,111) | 522 / 765 / 1,205 | 1,038 / 5 / 12 / 213 / 998 | 341 / 4 / 5 / 73 / 361 | 4.2 / 1.7 | 2,709 / 2,942 (42.1) | 1,458 / 1,613 (22.7) | 21.4 (335) | 0 |
| mixed+hostile/64 | 1,059 / 2,301 / 6,247 (11,111) | 512 / 1,085 / 1,518 | 1,114 / 70 / 11 / 197 / 970 | 674 / 56 / 5 / 72 / 371 | 3.8 / 1.7 | 2,714 / 12,345 (44.1) | 1,462 / 11,073 (24.6) | 21.4 (335) | 0 |
| mixed/128 | 3,919 / 5,332 / 6,566 (16,666) | 1,505 / 2,452 / 3,893 | 2,363 / 6 / 26 / 562 / 2,522 | 1,042 / 5 / 20 / 259 / 1,150 | 12.0 / 4.9 | 6,763 / 7,420 (52.3) | 3,546 / 3,853 (27.4) | 58.2 (455) | 0 |
| mixed+hostile/128 | 3,627 / 5,347 / 8,920 (16,666) | 1,517 / 2,355 / 3,890 | 2,546 / 143 / 26 / 556 / 2,496 | 1,075 / 59 / 18 / 255 / 1,141 | 11.8 / 4.8 | 6,771 / 16,556 (53.3) | 3,552 / 13,335 (28.4) | 58.2 (455) | 0 |
| listen-uplink/16 (listen) | 91 / 265 / 362 (8,333) | 58 / 171 / 409 | 210 / 7 / 0 / 14 / 73 | 150 / 4 / 0 / 9 / 76 | 0.3 / 0.2 | 408 / 755 (26.1) | 256 / 399 (16.2) | 0.55 (34) | **108 (before and after; see below)** |
| listen-uplink/32 (listen) | 385 / 535 / 1,014 (8,333) | 243 / 343 / 661 | 223 / 3 / 3 / 66 / 252 | 133 / 2 / 1 / 31 / 189 | 1.2 / 0.8 | 1,511 / 1,594 (45.4) | 784 / 861 (23.8) | 12.7 (398) | 0 |

**CPU per seat** (the server thread's share of one core, per honest client): 0.045 / 0.066 / 0.094 % at 40 / 64 / 128
before, 0.030 / 0.027 / 0.038 % after. All three tiers are met with room: tick p99 is 6 %, 7 % and 15 % of its budget
at 40, 64 and 128. What still rises at 128 is the sample game's own traffic (455 kbit/s per seat against 335: every
avatar goes to every client).

The perf row (`scale_tick_p99_tiers`: `mixed`, 60 virtual s per tier), three rounds on the same core with before and
after interleaved (1-minute load 14-18):

| Tier | Budget ms | Before: tick p99 ms (CPU % of a core) | After: tick p99 ms (CPU % of a core) | Allocs/tick p50 before / after |
|---|---|---|---|---|
| 40 | 8.333 | 0.692, 0.774, 0.612 (1.3, 1.3, 1.2) | 0.469, 0.412, 0.436 (0.9, 0.9, 0.9) | 1,696 / 912 |
| 64 | 11.111 | 1.766, 1.258, 1.512 (3.0, 2.6, 2.6) | 0.680, 0.757, 0.805 (1.5, 1.6, 1.6) | 2,678 / 1,442 |
| 128 | 16.666 | 3.493, 5.217, 5.044 (8.1, 9.5, 9.2) | 2.038, 2.749, 2.482 (4.3, 5.4, 4.5) | 6,779 / 3,550 |

Where the 128-client server iteration went before ADR-10081 (timing scopes around the phases, a scratch build, the same
core, 60 virtual s; shares of the phase): flush: loss detection 28 %, sealing 35 %, the rest of packet building 20 %,
per-poll set-up 7 %, statistics 4 %; receive: acknowledgment processing 37 % (loss detection again), opening 22 %,
message decode and input storage 10 %, parsing 6 %. On the build server under load the loss detection shares were 46-48 % and
58 %: 128 sent-packet windows (about 16 MiB) do not stay in a contended core's cache. After ADR-10081 sealing and
opening are the largest single costs (about 1.85 µs per 1.2 KiB datagram sealed and 1.5 µs per client datagram
opened).

**`listen-uplink/16` fails on this base, before and after the cost changes alike** (fixed by ADR-10081 (5), next section): from
about 50 s, when two clients make their scripted rejoin, every client's send allowance falls to the floor (34 kbit/s
per seat, an entity budget of 8, staleness p99 54 ticks) and stays there, and clients 5 and 13 end with
`protocol_error` and rejoin every two seconds (108 ends). At ADR-8454's base the same run held 16 with 117 kbit/s per
seat and no ends. `listen-uplink/32` is clean. Reproduce:
`cargo xtask net-soak --profiles listen-uplink --clients 16 --modes listen --seed 1592590337 --seconds 180`.

### The same plan with ADR-10081 (5) (the shared uplink's queue trend read before the witness gate)

The failing row was bisected to the witness gate of ADR-8733 (2) (v06-net-soak-followups): it passes at the v0.6.0
tag. Ten seeds per listen-server row (60 virtual s, `listen-uplink/16` 180 s), runs whose gating bars all pass:

| Row | v0.6.0 tag | + ADR-8733 (2) | + ADR-10081 (5) |
|---|---|---|---|
| listen-uplink/16, 180 s | 9 | 0 | 10 |
| good-wifi/16 | 10 | 2 | 10 |
| mixed/16 | 0 | 2 | 10 |
| congested/16 | 1 | 6 | 8 |
| congested/8 | 0 | 10 | 10 (the same numbers run for run) |
| bad-wifi/8 | 6 | 10 | 10 (the same numbers run for run) |

At the plan's seed (60 virtual s), the 8- and 32-client listen rows of `listen-uplink`, `good-wifi`, `bad-wifi` and
`mixed` and `congested/8` give the same numbers with ADR-10081 (5) as with ADR-8733 (2) alone; `congested/32` passes
with both (46 / 51 kbit/s mean / max down per client against 39 / 44).

With ADR-10081 (5) the whole scale plan passes (same core, 1-minute load 29-32): `listen-uplink/16` holds 16 with 0
involuntary ends, staleness p99 2 ticks and 1.23 Mbit/s up (77 kbit/s per seat; tick p50 / p99 95 / 193 µs, 0.3 % of
a core); the other seven runs are identical to the base in every recorded field but wall time and allocations. The
gate plan's 61 runs are identical to the base in the same sense, before and after both changes.
