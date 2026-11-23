# Net evidence ledger

One row per tag (or integration line) from the 40-client scale run of the net soak (spec 120 § 1.1 "It scales", N215,
ADR-8454): `cargo xtask net-soak --plan scale-gate --jobs 1 --release --label "<tag> <host>"` (60 virtual s; or the
40-client rows of `--plan scale`, 180 s). The lead adds the row when a tag is cut; the full per-count table is
`docs/perf/net-scale-soak.md`. Wall-clock columns depend on the host and its load: write both.

Columns: tick p99 against the 40-client budget (8,333 µs); server CPU as a share of one core; server allocations per
tick (p50); server upstream in total and per seat; involuntary disconnects of honest clients; clients over their
bandwidth budget; the `mixed+hostile` run's hostile bars (`hostile_dropped`, `others_unaffected`, `no_hijack`).

| Tag / line | Head | Host (1-min load) | Run | Tick p99 µs (budget 8,333) | CPU % of a core | Allocs/tick p50 | Up Mbit/s (kbit/s per seat) | Involuntary ends | Seats over budget | Hostile bars | Listen-uplink/32 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| lane net-scale-soak (base 04d9ad838 + net-relevance-scale + net-v2-rate-controller-c) | see the lane report | the build server (170-205) | `--plan scale`, 180 s, virtual wire | 5,877 | 8.0 | 1,622 | 13.2 (330) | 0 | 0 | FAIL (dropped: no; others: client 0; hijack: client 0) until N103 + N105 | clean (0 ends, 0 over, rate within the uplink) |
| lane net-scale-soak (same build) | see the lane report | the min-spec Windows laptop, Windows 11 (quiet) | `--plan scale-gate`, 60 s, virtual wire | 2,154 | 2.8 | 1,650 | 13.6 (341) | 0 | 0 | FAIL until N103 + N105 (as above) | clean (listen-uplink/16 at 180 s: tick p99 398 µs, 0.7 % of a core) |
| lane v07-net-scale-128 (v0.6.0 tag + v06-net-soak-followups, net-rpc-p95-up-leg, v06-net-review-fixes, v06-security-gaps-fixes + ADR-10081) | see the lane report | the test machine, one core pinned (21-30 from other builds) | `--plan scale`, 180 s, virtual wire | 485 (before ADR-10081: 1,084) | 1.2 (before: 1.8) | 914 (before: 1,696) | 13.4 (335) | 0 | 0 | pass (dropped, others unaffected, no hijack) | clean at 32 (0 ends); listen-uplink/16: 108 `protocol_error` ends on this base (ADR-8733 (2)), 0 with ADR-10081 (5) |
