# Net soak

The net soak puts `regia_net`'s real server and clients on a bad network and fails when a player would lag out, be
dropped or get stuck (ADR-7899). Loopback tests show the protocol works when nothing goes wrong; the soak shows what
happens when the network misbehaves: loss in bursts, jitter, a full queue at a slow link, a path MTU below the packet
size, a dropped Wi-Fi link, a phone that changes address, and a peer that attacks the server.

The soak is a developer tool: a command, a test and the dev-only crate `regia_net_soak`. It is not a
runtime feature and never ships in a game, so it has no agent-protocol method. An agent runs the command and reads
the report files.

## Run it

```sh
cargo xtask net-soak                                      # the merge-gate set: 60 runs of 60 virtual seconds or more
cargo xtask net-soak --list                               # the profiles and the bars (owner, gating); nothing runs
cargo xtask net-soak --bars                               # the bars alone
cargo xtask net-soak --plan nightly --jobs 16             # every profile at 8, 32, 64, 128 clients, both server modes, 300 s
cargo xtask net-soak --plan scale --jobs 1 --release   # 40/64/128 clients on mixed and mixed+hostile, listen-uplink 16/32, 180 s
cargo xtask net-soak --plan scale-gate --jobs 1          # the gate-sized scale set: 40 and 32 clients, 60 s
cargo xtask net-soak --plan scale --wire udp --jobs 1    # the same over real UDP sockets on 127.0.0.1
cargo xtask net-soak --profiles flappy --clients 32 --seconds 120
cargo xtask net-soak --profiles hostile --clients 8 --modes dedicated,listen --seed 7
cargo xtask net-soak --no-run --out target/net-soak/20261115T201530Z   # re-read a report's verdict
```

Each invocation is one `cargo test --workspace --test net_soak` process (`regia_test_utils`'s `net_soak_matrix`,
which measures each run's heap with the counting allocator). Runs share
nothing and use virtual time, so `--jobs` (at most 16) changes only the wall time. The soak uses only the CPU. On a
shared build host, run it at low priority (`nice -n 15 ionice -c3`).

| Flag | Default | Meaning |
|---|---|---|
| `--plan gate\|nightly\|scale\|scale-gate` | `gate` | a named set; ignored when any custom flag is given |
| `--wire virtual\|udp` | `virtual` | the datagram path of every run: in-process queues, or the same network over real UDP sockets (see [Scale](#scale)) |
| `--profiles a,b` | all | a custom set's profiles |
| `--clients 8,32` | 8 | a custom set's client counts |
| `--modes dedicated,listen` | dedicated | a custom set's server modes |
| `--seconds N` | 60 | virtual seconds per run of a custom set |
| `--seed S` | `0x5EED0001` | the seed of every random stream |
| `--jobs N` | 8 | worker threads, at most 16 |
| `--out DIR` | `target/net-soak/<UTC time>` | where `result.json` and `summary.md` go |
| `--release` | off | build the soak in release (the dev profile already optimizes dependencies) |
| `--light` | off | run `regia_net_soak`'s own matrix test: builds only `regia_net` and the harness, reports no heap numbers |
| `--label TEXT` | none | a label for the report (a commit, a host) |
| `--no-run` | off | read `--out`/`result.json` and exit with its verdict |
| `--list` | off | print the profiles (and which are skipped until an engine part lands) and the bars; run nothing |
| `--bars` | off | print the bars: id, whether it gates, the owning lane, the profiles it judges, the rule; run nothing |

Exit codes: `0` every bar passed, `1` a bar failed, `2` the harness itself failed (the test process did not finish).

## What runs

**The impairment network** (`regia_net_soak::impair`) is an in-process datagram network. It plugs in at
`regia_net`'s transport seam: every endpoint gets an `ImpairedSocket`, a `DatagramSocket` like `UdpTransport`, so
`NetServer` and `NetClient` run unchanged, with encryption on as shipped (the addresses are not loopback). Every
endpoint has an access link with an up and a down direction. A datagram crosses the sender's up direction and then
the receiver's down direction. Each direction applies, in this order: outage, MTU, random loss, Gilbert-Elliott burst
loss, a bandwidth bottleneck behind a drop-tail queue, latency plus jitter, reordering and duplication. NAT rebinding
moves an endpoint to a new public port, and datagrams for the old port are dropped. Time is virtual. Every random
decision comes from a SplitMix64 stream per endpoint and direction, seeded from the run seed, so the same seed gives the
same run on every machine.

- Jitter keeps the order of the datagrams, as on one network path. Reordering is a separate setting, so its rate is
  the stated one.
- The bandwidth cap counts 28 bytes of IPv4 and UDP header per datagram. A datagram that would find more than the
  queue size waiting is dropped.
- MTU: a UDP payload longer than the MTU is dropped, because a path that drops IP fragments does the same.
- Phases (ADR-8030): a link can switch both directions to new numbers at set times (an RTT step, an MTU that
  shrinks, Wi-Fi to cellular). The queue's backlog and the burst chain carry over the switch.
- Address moves: besides a new port, an endpoint can move to another public IP at a set time (Wi-Fi to cellular, a
  client moving behind a carrier-grade NAT). `ImpairedNet::addresses_held(ep)` lists every address an endpoint held.
- Cross traffic: a direction with `cross_bps` feeds 1,500-byte filler datagrams at that constant rate into its own
  bottleneck queue (another device's download on the same home link). The queue starts full and stays full when the
  filler is faster than the bottleneck, so a game datagram that finds room waits for about the whole queue. The
  filler's phase comes from the direction's seeded stream.
- Clock rate: each endpoint's clock runs at `1 + clock_ppm / 10^6` of the virtual clock (`LinkSpec::local_ms`).
  The workload gives each `NetClient` its local time, and the client's frames run every 16 ms of local time.
- Forged sources: `ImpairedNet::send_forged(ep, from, bytes, dest)` sends over `ep`'s link with any source address.

**The workload** (`net_soak::workload`) is one `NetServer` at 30 Hz and N `NetClient`s of a sample game. Each client:

- joins in the first seconds;
- sends one input per server tick through the engine's own loop (`NetClient::due_input_ticks`: every tick once, up
  to its tick estimate plus the lead steered from the server's input feedback, ADR-8077), with redundancy 15 (DO27's
  setting). Most inputs
  have no command tail. 15 % carry 8 to 64 bytes and 3 % carry 0.5 to 1 KiB. Every eighth client (index 2, 10, ...)
  sends a 548 B to 1 KiB tail on every tick from 20 % to 30 % of the run;
- predicts its avatar and counts a correction whenever the server disagrees;
- sends a reliable 0.5 to 4 KiB command every 2 to 4 s and a chat line every other command;
- leaves at 30 % of the run if it is every eighth client (index 5, 13, ...), and rejoins 2 s later on a new port;
- after a timeout, while it holds a resume ticket (the profile's server keeps tickets: `cgnat`, `flappy`,
  `long-outage`), runs the engine's reconnect timeline (`regia_net::reconnect::Reconnector`, ADR-8352, the schedule a
  `NetLink` or `NetSession` client runs: the first attempt 1 s after the timeout, doubling to 8 s, for 60 s), each
  attempt a fresh handshake on a new port that presents the ticket; the first accept is recorded as `Restored`. At
  the window's end it gives up and joins afresh (ADR-10093);
- reconnects at once, on a new port, after any other involuntary end, and after a timeout without a ticket.

The server of `cgnat`, `flappy` and `long-outage` keeps resume tickets for 60 s (`ServerConfig::resume_grace_ms`, the
`NetLink` default): a player that times out is dormant, its avatar stays in the world, nothing is sent to it and none
of its ticks are judged; when it resumes it gets the world blob again, as a join does. Every other profile's server
keeps none, so its runs are those of the bare transport.

A profile may also add a bulk transfer: on `+bulk` runs the server sends every client 256 KiB as 32 KiB reliable
messages at half the run. `flood` adds a peer that sends 100,000 valid sealed datagrams a second for 10 s from 10 s,
and the hostile bars judge it. `forged-handshake` replays real `ConnectRequest` datagrams 100,000 times a second from
forged sources in 1,000 /24 prefixes. On every iteration, the server's address for each peer is checked against the
addresses that peer's client really held.

The server simulates the inputs that arrived; a missing input moves nothing. It sends a 16 KiB world blob on join (on
the replication control stream, a foreground stream: a join snapshot never waits behind state frames, ADR-8624) and
a snapshot every tick. The snapshot has the client's own position and up to about 140 entities (avatars plus at least
64 NPCs) in parts of 80. A per-client entity budget sizes it from the transport's send allowance for that client
(`NetServer::send_allowance` over the tick rate, at least 8 entities; ADR-8301). The server answers every command
with a reliable acknowledgement.

**Server modes.** `dedicated`: the server's own link is clean. `listen`: the server sits behind a home link with 5 ms
one way, 20 Mbit/s up shared by every client behind a 128 KiB drop-tail queue, and 100 Mbit/s down. The host's own
player does not cross the network.

**Steam transport.** The adapter is covered without Steam: `crates/regia_steam/tests/net_soak_steam.rs` runs
`regia_steam::SteamSocket` over a `P2pMessages` backed by the impairment network. A server and four clients play on
Steam's synthetic addresses under `bad-wifi`, with encryption on, and every client must stay connected with its
inputs reaching the server. Steam's relay network itself is not covered: it needs a Steam client and an account. The
owner's two-account session checks it.

## Profiles

Every number is one way, per direction, for each client's link. `up` is what the client sends.

| Profile | Latency + jitter | Loss | Reorder / dup | Bandwidth (up / down) and queue | Other |
|---|---|---|---|---|---|
| `clean` | 1 ms | none | none | unlimited | |
| `good-wifi` | 4 ms + 0-4 ms | 0.2 % | 0.1 % (10 ms) / 0.05 % | 20 / 50 Mbit/s, 64 KiB | |
| `bad-wifi` | 8 ms + 0-30 ms | 1 % random + Gilbert-Elliott (good→bad 0.01, bad→good 0.3, 60 % loss in bad; about 3 % in all) | 1 % (30 ms) / 0.5 % | 2 / 6 Mbit/s, 64 KiB | |
| `mobile-4g` | 35 ms + 0-25 ms | 0.5 % + Gilbert-Elliott (0.005, 0.2, 50 %) | 0.5 % (40 ms) / 0.1 % | 2 / 8 Mbit/s, 128 KiB | MTU 1232; one NAT rebinding per client at 40 % of the run |
| `transatlantic` | 45 ms + 0-6 ms | 0.3 % | 0.2 % (15 ms) / none | 20 / 50 Mbit/s, 64 KiB | |
| `congested` | 15 ms + 0-10 ms | none (the queue drops) | none | 1 Mbit/s / 256 kbit/s (below the game's ~450 kbit/s), 16 KiB | |
| `flappy` | as `good-wifi` | as `good-wifi` | as `good-wifi` | as `good-wifi` | a 3 s outage every 20 s from 15 s; one 22 s outage (2 s beyond the engine's 20 s removal timeout; derived from `regia_net::TimeoutPolicy::default()`) at half the run on every fourth client (index 3, 7, ...) |
| `hostile` | as `good-wifi` | as `good-wifi` | as `good-wifi` | as `good-wifi` | one hostile peer, below |

The twenty profiles of spec 120 § 15.1 (ADR-8030). Unless a row says otherwise, the other numbers are
`good-wifi`'s:

| Profile | Numbers | Bars it feeds |
|---|---|---|
| `high-rtt` | 225 ms (even clients) or 325 ms (odd) one way, so 450 or 650 ms RTT; 0-20 ms jitter; 1 % loss; unlimited bandwidth | |
| `rtt-step` | 20 ms + 0-5 ms one way; 125 ms from 60 s; 20 ms again from 120 s; runs at least 180 s in the plans | |
| `clock-drift` | `good-wifi`; even clients' clocks run 0.5 % fast and odd clients' 0.5 % slow (3 s over 10 min) | |
| `nat-rebind` | `good-wifi`; each client's source port changes every 30 s with no outage | `no_hijack` |
| `cgnat` | `good-wifi`; in each group of 10 clients nine share one public IPv4 on distinct ports, and the tenth moves in from another IP at 40 % of the run; at least 10 clients in the plans | `no_hijack` |
| `wifi-to-cell` | Wi-Fi (15 ms, 0.5 % loss) until 40 % of the run; then a 2 s outage, a new public IP, and cellular (60 ms, 3 % loss) | `resume_same_peer` |
| `long-outage` | `good-wifi`; a two-way outage at 60 s of 11 s, 25 s or 45 s (client index mod 3); runs at least 120 s in the plans | `interrupted_once`, `resume_same_peer` |
| `bufferbloat` | 1 Mbit/s each way behind a 1 s (125,000-byte) drop-tail queue, no random loss, 20 ms one way | `rate_vs_bottleneck`, `queue_delay_p95` |
| `downstream-loss` | 30 ms one way; no loss up, 15 % random loss down; unlimited bandwidth | |
| `asym-bufferbloat` | `good-wifi` up; down 4 Mbit/s behind a 1 s (500,000-byte) queue kept full by cross traffic at 4.4 Mbit/s; the server keeps reconnect tickets for 60 s, so the scripted rejoin is a one-round-trip ticket resume (ADR-8902) | `missed_input` |
| `listen-uplink` | `good-wifi` clients; the server's uplink is 20 Mbit/s shared by every client behind a 128 KiB queue, in either mode | `rate_vs_bottleneck` |
| `lossy-fat-pipe` | 100 Mbit/s each way, 5 % random loss, 20 ms; a 64 KiB buffer (5 ms at that rate), so no standing queue | `random_loss_kept` |
| `rtt-extreme` | `good-wifi`, except one client in 8 on 1,500 ms one way with 20 % loss each way | `never_kicked_for_ping` |
| `mtu-1280` | `good-wifi` with a path MTU of 1,280 bytes | |
| `mtu-blackhole` | `good-wifi` with MTU 1,280, then 1,200 from 30 s; a bigger datagram is dropped without a trace | |
| `flood` | `good-wifi` plus a peer sending 100,000 valid sealed datagrams a second from 10 s for 10 s | `hostile_dropped`, `others_unaffected` |
| `forged-handshake` | `good-wifi` plus 100,000 forged `ConnectRequest`s a second from 1,000 /24 prefixes, the whole run | `join_under_flood` |
| `relay-path` | every client through the relay, `good-wifi` on each leg. **Skipped: needs relay v2** | |
| `lockstep-mix` | a lockstep match, not a replication run ([Lockstep runs](#lockstep-runs)): the seats sit on `good-wifi`, `bad-wifi`, `mobile-4g` and `transatlantic` in turn and send the order-driven input mix; runs 300 s in the plans, at up to 40 seats | `desync_zero`, `lockstep_upload` |
| `mixed` | 40 % `good-wifi`, 25 % `bad-wifi`, 15 % `mobile-4g`, 10 % `transatlantic`, 5 % `flappy`, 5 % `congested` | `tick_p99` |
| `congested+bulk` | `congested` plus the bulk transfer at half the run (any replication profile takes the `+bulk` suffix) | `rpc_p95_with_bulk` |
| `lockstep-mix+action` | `lockstep-mix` on the same links with the action input mix: half the seats change their aim on any tick (only `lockstep-mix` takes the `+action` suffix) | `desync_zero`, `lockstep_upload` |
| `lockstep-mix+bots` | `lockstep-mix` on the same links with the wander-fire bot mix: a seeded heading held 60 to 180 frames, a fire pulse every 120 to 300 frames, and a lead on a hull inside 200 m (only `lockstep-mix` takes the `+bots` suffix; ADR-8409). The gate runs it once, at 40 seats for 600 s | `desync_zero`, `lockstep_upload` |

A mixed profile gives client `i` its share by a smooth weighted round robin over the index. The order is fixed, and
any first `n` clients hold the shares as closely as whole clients allow: 40 `mixed` clients are exactly 16/10/6/4/2/2.
A skipped profile still appears in the report with its reason, and its bars are listed as skipped.

**The hostile peer** runs from 10 s. It has three connected sessions of 10 s each, on new ports:

- `over_cap_input`: one input per frame whose command tail claims 60,000 bytes (the cap is 1,024) inside an
  otherwise valid record.
- `malformed_messages`: one undecodable game message per frame, and a 60 KiB undecodable reliable message every
  second (it is reassembled before it is refused).
- `flood`: 3,000 valid unreliable chat messages per frame plus as many reliable ones as the window takes.

During all three sessions, a second address also replays the last 16 datagrams of client 0 every 96 ms. It also sends
8 random datagrams of 1 to 1,400 bytes per frame and one 4 KiB to 60 KiB datagram every 96 ms.

## Measurements

`result.json` (format `regia-net-soak` v2 since ADR-8030; the reader still loads v1) has one entry per run. Each entry has the scenario, the bars, the metrics,
every involuntary end, every rejoin, every wedge, the hostile record and the reproduction command. `summary.md` has a
table and every failed bar with its seed and command.

| Metric | Meaning |
|---|---|
| `ends_by_cause` | involuntary ends by side and code (`client:timeout`, `server:malformed_packet`, ...); `ends_excused` counts the engine's own removals: ends inside an outage longer than its removal timeout (20 s), no more than 1 s before that timeout has run |
| `reconnects`, `reconnect_max_ms` | rejoins, and the longest time from the link being usable to connected |
| `connect_refusals`, `connect_timeouts` | join or rejoin attempts the server refused (`full`, ...) or that timed out; a refused attempt is not a lost connection, the rejoin bar judges it |
| `inputs_sent`, `input_send_errors` | inputs the transport took, and its refusals by kind (`too_large`, `backpressure`, ...) |
| `server_ticks_with_input`, `server_ticks_missed`, `worst_missed_fraction` | ticks the server simulated with or without a client's input (lost or late; outages excluded) |
| `inputs_stamped_behind` | inputs the client stamped for a tick its estimate said the server had already simulated |
| `corrections`, `correction_rate`, `correction_steps_*`, `rollback_ticks_*` | mispredictions, their size in grid steps, and how many predicted ticks each one rewound |
| `staleness_ticks_*` | the client's estimate of the server tick minus the newest snapshot tick, sampled every frame |
| `commands_*`, `command_rtt_p99_ms` | reliable commands and their acknowledgement round trip |
| `down_bps_*`, `up_bps_*`, `*_budget_bps` | per-client rates against the game's budget (1 Mbit/s down, 256 kbit/s up; the cap on `congested`) |
| `min_entity_budget` | the smallest snapshot entity budget any client got after 5 s |
| `server_tick_us_*` | wall time of a server iteration that ran a tick (receive, simulate, snapshot, flush); host-dependent, never a bar |
| `peak_heap_bytes` | the run thread's peak heap growth: server, clients, network and reassembly buffers |
| `drops` | network drops by cause: `random`, `burst`, `queue`, `mtu`, `outage`, `no_route`, `inbox` |
| `scale` | the scale numbers of [Scale](#scale) (ADR-8454; absent in older files) |

### Staleness by stage (ADR-10145)

`measures.staleness_stages` says where each limit group's staleness went. A sample of `staleness_ticks_*` is the client's
tick estimate minus the tick of its newest snapshot; the soak traces that snapshot to the datagram that carried it and
splits the sample into seven stages that add up to it exactly:

| Stage | From, to |
|---|---|
| `sender_hold` | the server tick that made the snapshot, to its datagram leaving the server's socket (send queue, pacer) |
| `queue` | what the datagram waited in the bottleneck queues of its route |
| `serialize` | its own transmission time at those bottlenecks |
| `path` | latency, jitter and any reordering hold |
| `receiver_hold` | arrival in the client's socket, to the frame that handed the snapshot to the game |
| `wait` | that frame, to the sample: the snapshot interval and every snapshot that did not arrive (lost, expired in the sender, withheld) |
| `estimate` | the bar's whole-tick count minus the snapshot's true age: the tick label (a snapshot of tick `t` is made when the server is at `t + 1`), rounding, and the error of the client's tick estimate; negative when the estimate runs behind |

Each group has `limit_ticks`, `p99_ticks` (what `staleness_p99` judges), `samples` (traced) and `untraced` (the real UDP
wire and capture replays cannot be traced), `over_limit_samples`, `mean` (the mean ticks per stage of every traced
sample), `tail` and `tail_samples` (the same for the samples at or over the p99: where the judged ticks went),
`dominant` (the tail's largest stage) and `settled_p99_ticks` with `settled_samples` (the samples 5 s or more after their
connect: the run without its join transients). `summary.md` lists the groups over their limit under "Staleness by
stage". It is a report for diagnosis: no bar reads it, and an older `result.json` loads with an empty list.

Reading it: `queue` large with the rest small is a standing queue (the sender's own when a lower rate would empty it,
other traffic's when not: `asym-bufferbloat` reads 29.9 ticks of queue and an estimate 15 ticks behind); `wait` large is
loss or a sender that held snapshots back; `sender_hold` should stay near zero. In a test,
`regia_net_soak::workload::Knobs::start_bytes_per_second` sets the rate controller's start rate for a run,
`Knobs::first_flight` switches on the first flight of ADR-10156 on every endpoint (off, the engine's default, in every
plan), and `ClientRecord::world_after_connect_ms` is how long each join's world blob took
(`tests/staleness_stages.rs`).

Version 2 adds `measures` to each run: the numbers the spec 120 bars read. They are staleness with the link up
grouped by limit, each finite bottleneck's seconds over its rate, the queueing-delay p95, command round trips before
and during the bulk transfer, the world size, every outage with its `Interrupted`/`Restored` counts, every involuntary
rejoin with the peer id before and after, the server's address moves (and those to an address the client never
held), join attempts and how many connected within 5 s, and the forged requests sent. Each bar has `gating` and
`skipped`, each run has `skipped`, and the report has `non_gating_failures`. `SoakReport::from_json` reads both
versions. A v1 file keeps `version: 1` and its verdict, and gets the spec 120 bars its numbers allow. The other bars
are skipped with `not measured in a v1 result`. A lockstep run has `lockstep` where a replication run has `measures`
([Lockstep runs](#lockstep-runs)).

### Staleness: the path and what is added

`staleness_p99` counts the client's estimate of the server tick (`NetClient::estimated_server_tick`) minus the tick
of its newest snapshot, sampled every client frame with the link up. The path's one-way latency is inside every
sample: no sender delivers a snapshot sooner. A link of 325 ms one way reads at least 10 ticks at 30 Hz, and the
limit is 4. So each run also reports the same samples with the path taken off (ADR-10096). `measures.staleness_paths`
has one entry per sub-profile of the run:

| Field | Meaning |
|---|---|
| `profile`, `clients`, `limit_ticks`, `samples` | the sub-profile, its clients, their `staleness_p99` limit and the link-up samples |
| `p99_ticks` | the p99 as the bar counts it |
| `beyond_path_p99_ticks`, `worst_client_beyond_path_p99` | the p99 with the whole ticks of the path's one-way latency (the server's uplink plus the client's downlink, in force at the sample) taken off every sample, over the group and for its worst client |
| `path_ticks_min`, `path_ticks_max` | the fewest and most whole ticks taken off |
| `mean_ticks` | the mean of the bar's samples; the five means below add up to it |
| `mean_path_ticks` | the path's one-way latency |
| `mean_transit_ticks` | the newest snapshot's way from the server's send to the client frame that read it, beyond the path: jitter, queueing, anything the sender held it for, the wait for the client's frame |
| `mean_hold_ticks` | how long the newest snapshot had been the newest: the send interval, and lost or late snapshots |
| `mean_rounding_ticks` | counting in whole ticks: the server is on tick `t + 1` from the moment it sends the snapshot of tick `t` (half a tick on average) |
| `mean_estimate_ticks` | the client's tick estimate minus the tick the server is really on (negative: the estimate runs behind) |

The harness knows what a client cannot: when the server sent each snapshot, the server's real tick and each link's
latency. `summary.md` has a table "Staleness over the limit: the path and what is added" with every sub-profile
whose p99 is over its limit. No bar reads these numbers; `staleness_p99` is judged as before. A snapshot of the
sample game travels in two parts and either part counts as the newest, so a lost datagram rarely shows as hold.

## Scale

Spec 120 § 1.1 promises that a dedicated server keeps every player at 64 clients on the mixed bad-network profile
(gate-sized at 40, 128 recorded) and that a listen server holds 32 on a 20 Mbit/s uplink (N215, ADR-8454). The
`scale` plan runs `mixed` and `mixed+hostile` at 40, 64 and 128 clients and `listen-uplink` at 16 and 32 on a listen
server, 180 virtual seconds each; `scale-gate` runs the 40 and 32 runs for 60 s. `mixed` is 40 % good-wifi, 25 %
bad-wifi, 15 % mobile-4g, 10 % transatlantic, 5 % flappy and 5 % congested clients; `<profile>+hostile` adds the
`hostile` profile's peer to any profile, judged by `hostile_dropped`, `others_unaffected` and `no_hijack`
(`others_unaffected` leaves out the seconds inside a client's own outages and 1 s after them).

Each run's `scale` entry (and the summary's "Scale numbers" table for runs of 16 clients or more) holds:

| Field | Meaning |
|---|---|
| `tick_us_p50`, `tick_us_p99`, `tick_us_max`, `tick_budget_us` | server tick wall time and the run's `tick_p99` tier budget |
| `server_cpu_pct_one_core` | every server iteration's wall time over the run's length: the server's share of one core at real time |
| `allocs_per_tick_p50/p99/max`, `allocs_per_tick_per_client` | the server's allocations per tick, the impairment network's own datagram copies left out; `None` without a counting allocator |
| `server_up_mbps`, `server_up_kbps_per_seat`, `server_down_kbps_per_seat` | the server's traffic with 28 bytes of IPv4 and UDP header per datagram, in total and per honest client |
| `down_bps_max`, `up_bps_max`, `seats_over_budget` | the highest client's mean rates and every client over its budget (1 Mbit/s down, 256 kbit/s up) |
| `end_causes`, `involuntary_disconnects` | every end by `side:cause` (voluntary leaves included) and the involuntary ends not excused by a long outage |
| `rss_kib` | the process's resident set at the end and its peak, KiB (Linux; the whole process, clients included) |
| `wire_stats` | the real UDP wire's counters: datagrams sent and read, reads that did not match the expected length, reads from an unknown port, send errors, datagrams the kernel dropped |

Allocations are counted when the binary installs a counter with `regia_net_soak::probe::install`: the full matrix
(`regia_test_utils`) and `regia_net_soak`'s `scale` test binary do; `--light` does not. To run a plan through the
scale binary directly: `REGIA_NET_SOAK_OUT=<dir> REGIA_NET_SOAK_PLAN=scale REGIA_NET_SOAK_JOBS=1 cargo test --release -p
regia_net_soak --test scale -- --exact net_soak_scale_from_env --nocapture` (add `REGIA_NET_SOAK_WIRE=udp`).

Two scale tests count work instead of timing it (ADR-10081), so they run in `cargo test`:
`cargo test -p regia_net --lib scale_work_tests` (a connection's poll examines only sent packets whose fate is open)
and `cargo test -p regia_net_soak --test scale the_server_allocates_within_its_budget_per_client_per_tick` (at most 30
allocations per client per tick at 40 clients on `mixed`). The wall-clock tiers stay with the ignored perf row:
`cargo test --release -p regia_net_soak --test scale -- --ignored --nocapture --test-threads=1 scale_tick_p99_tiers`,
one run at a time on a quiet core (`taskset -c <core>`); a loaded host can triple the numbers.

**The real UDP wire** (`--wire udp`, `ImpairedNet::with_real_udp`): every endpoint also owns a non-blocking UDP
socket on 127.0.0.1, and every delivered datagram crosses the kernel's loopback path: the network sends it from the
sender's socket to the receiver's, and the receiver's `recv_from` reads it with a real system call. The impairments,
the seeded schedule and the virtual addresses the engine sees are those of the virtual wire, so encryption, rebinding
and the address checks behave the same; the run adds the system calls, the kernel's copies and its receive buffer. At
most 96 datagrams and 48 KiB wait unread in one receive buffer (under Windows' 64 KiB default); the rest wait in the
network until the reader finds its socket empty. `wire_stats.kernel_drops` counts datagrams the kernel dropped anyway.
It runs on Linux and Windows. The run's label ends in `/udp`.

The 40-client numbers go into the net evidence ledger (`docs/releases/net-evidence-ledger.md`) at each tag, and the
`tick_p99` tiers are the `net_scale_tick_p99_*` rows of `docs/perf/budgets.ron`
(`cargo test --release -p regia_net_soak --test scale -- --ignored --nocapture scale_tick_p99_tiers`).

## Bars

| Bar | Rule |
|---|---|
| `involuntary_disconnects` | no connection lost that the workload did not end (counted once: the client's end, or the server's when the client never noticed), unless it is the engine's own removal: inside an outage longer than its removal timeout (20 s, `regia_net::TimeoutPolicy::default()`) and no more than 1 s before that timeout has run (`flappy` and `long-outage` have such outages) |
| `reconnect_within_grace` | every rejoin is connected within 3 s of its link being usable |
| `no_wedged_client` | every client joins. No connected client goes 2 s, with its link up, without its input reaching the server (`server_no_input`; a peer none of whose inputs has arrived yet counts from 1 s after its join, ADR-8424, whether its first input, its removal or the run's end closes the gap, ADR-10107; a gap is judged up to the client's own leave or end even when the server removes the peer first, ADR-8887), without a snapshot (`no_snapshot`), or with every input send failing (`input_send_failing`). An outage a gap overlaps excuses only itself and the 1 s after it (`INPUT_GRACE_MS`): the rest of the gap is judged, and a gap that starts after an outage has ended is judged whole (ADR-8887; before, any outage a gap touched excused all of it) |
| `hostile_dropped` | (`hostile`) the server drops every hostile session within 5 s of its first hostile act |
| `others_unaffected` | (`hostile`) no honest client has an involuntary end or a wedge while the hostile peer is active, or in the 20 s after (the engine's removal timeout), and no honest client's snapshot staleness in that time rises more than 10 ticks (333 ms) above its own worst in the first 10 s, before the hostile peer starts |
| `bandwidth_within_budget` | (`congested`) each client's offered downstream rate over the second half of the run is at most the 256 kbit/s cap |

Spec 120 § 15.2 adds fifteen bars (ADR-8030). Each is a function of the run's `result.json` entry
(`regia_net_soak::report::spec120_bars`). Each is reported on every run whose profile it names, and none gates until
the lane that owns it sets `gating: true` in `xtask/src/net_soak.rs` `BARS`. That lane must flip it in the commit that
shows the bar green on its profiles, and never by moving the bar. `cargo xtask net-soak --bars` prints the table.
`lockstep_upload` gates since ADR-8209, `interrupted_once` and `resume_same_peer` since ADR-10093 (green on every
gate row of their profiles once the outage rows ran the resume); the other twelve do not gate yet.

| Bar | Pass when | Profiles | Owner (spec 120 § 17) |
|---|---|---|---|
| `missed_input` | the run's missed input ticks are at most 0.5 % (clean, good-wifi), 2 % (bad-wifi, mobile-4g) or 3 % (asym-bufferbloat) | those | N111 net-input-feedback |
| `corrections` | corrections are at most 1 % of compared predictions on good-wifi, 3 % on bad-wifi | those | N207 net-clock-filters |
| `staleness_p99` | snapshot staleness p99 with the link up is at most 4 ticks; 8 for congested clients | every run | N202 net-v2-rate-controller |
| `rate_vs_bottleneck` | from 5 s after each connect, every whole second's offered wire rate (payload plus 28 bytes per datagram) at each finite bottleneck is at most its rate | congested, bufferbloat, listen-uplink | N202 |
| `queue_delay_p95` | the game's datagrams wait at most 100 ms p95 at the bottleneck queues | bufferbloat | N202 |
| `rpc_p95_with_bulk` | command round trip p95 while the bulk transfer is in flight is at most 1.5x the same run's p95 from 5 s to the bulk start; an unanswered command counts until the run end | congested+bulk | N203 net-v2-streams |
| `random_loss_kept` | the smallest snapshot entity budget after 5 s, outside a peer's first flight (ADR-10200: its allowance is the floor by design; the first flight is opt-in, so no plan flies one), is at least 90 % of the full world (a clean run's budget) | lossy-fat-pipe | N202 |
| `interrupted_once` | one `Interrupted` and one `Restored` per outage of at least 1.5 s (the `interrupt_ms` default) while connected, none elsewhere; an outage the client left on purpose inside owes no `Restored` | flappy, long-outage, good-wifi | N101 net-timeout-policy |
| `resume_same_peer` | every involuntary rejoin gets its old `PeerId` back, and so its avatar | long-outage, wifi-to-cell | N216 net-link-resume-dormant |
| `no_hijack` | the server never moves a peer to an address its client did not hold | hostile, nat-rebind, cgnat | N103 net-rebind-path-validation |
| `never_kicked_for_ping` | every rtt-extreme peer joins and is never disconnected | rtt-extreme | N101 |
| `desync_zero` | 0 desync verdicts and 0 state transfers (seats and host), no seat removed, the votes decided, every simulation in step at the last tick they all stepped, and at least one checkpoint agreed | lockstep-mix, lockstep-mix+action, lockstep-mix+bots | N226 net-desync-detect |
| `lockstep_upload` | the host's upload with IP and UDP headers, over the whole seconds after the match starts: the 95th-percentile second is at most 4.0 Mbit/s and the busiest second at most 4.4 Mbit/s. Stated over a 5-minute run, at up to 40 seats on the orders mix and the bots mix and 24 on the action mix; a run with more seats reports its numbers and skips the bar | lockstep-mix, lockstep-mix+action, lockstep-mix+bots | N224 net-lockstep-core |
| `tick_p99` | the server tick's wall-time p99 is within its tier's budget: a quarter of the 30 Hz tick (8,333 µs) up to 40 clients, a third (11,111 µs) up to 64, half (16,666 µs) above (`docs/perf/budgets.ron` `net_scale_tick_p99_40`/`_64`/`_128`, ADR-8454); a wall-clock bar, judged on a quiet host one run at a time (`--plan scale --jobs 1`) | mixed | N215 net-scale-soak |
| `join_under_flood` | at least 99 % of join attempts connected within 5 s | forged-handshake | N218 net-handshake-availability |

The engine raises `Interrupted` and `Restored` (ADR-7950) and the workload records them. An outage beyond the removal
timeout (`flappy`'s 22 s, `long-outage`'s 25 s and 45 s) ends the connection at 20 s of silence on both sides; the
server holds the player dormant, the client's reconnect timeline presents its resume ticket, and the first accept
after the link returns is the same `PeerId` and the outage's one `Restored` (ADR-10093). Before ADR-10093 the soak's
server kept no tickets and its client joined afresh, so both bars failed on those outages; they still fail on a
client that does not reconnect or a server that does not hold the slot (`Knobs::reconnect`,
`Knobs::server_resume_grace_ms` in the kit's tests). An outage the client left on purpose inside (the scripted leave
of the gate's `flappy` row falls in one) owes no `Restored`: the link is closed. `summary.md` lists the
non-gating bars in their own table, and `cargo xtask net-soak` prints them after the verdict.

### Lockstep runs

A run of `lockstep-mix`, `lockstep-mix+action` or `lockstep-mix+bots` is a lockstep match (ADR-8146, ADR-8209, ADR-8409), not the sample game: the
plan hands it to `regia_net_soak::lockstep::run_lockstep`. `--clients` is the number of seats. On a dedicated server
the host is an observer that steps its own simulation and votes; on a listen server the host plays seat 0. The
settings are `LockstepConfig::default()`. In `result.json` such a run has a `lockstep` block instead of `measures`
(the format version stays 2; the key is absent from older results, which still load): seats, input mix, frames built,
the input delay (at the end of the run, its raises and lowers, its mean and its largest value over the run), the
host's upload (mean, median, 95th-percentile and busiest second, the seconds judged and how many were over 4.0
Mbit/s, datagrams per second), desync verdicts and state transfers, the worst seat's ticks without input, the worst
stall after a single lost datagram, stream frames, hole repairs, checkpoints agreed, the seats that ended, never
connected, fell behind or were dropped, and `late_stamp_flagged`: the seats the host flagged for holding their
inputs back ([late stamping](net-lockstep.md#late-stamping)); every seat of a soak run is honest, so a seat listed
there is a false flag to report. `summary.md` has a `Lockstep runs` table with these and both lockstep bars'
verdicts.

The bars on a lockstep run:

- `involuntary_disconnects` fails when a seat's connection ended. `no_wedged_client` fails when a seat never
  connected, stepped more than 60 frames fewer than the host built, or was dropped as lagging.
  `reconnect_within_grace` passes as not applicable (the lockstep workload never rejoins).
- `desync_zero` and `lockstep_upload` are judged from the `lockstep` block. `lockstep_upload` is judged up to the
  seat count it is stated for on the run's input mix (`LOCKSTEP_UPLOAD_SEATS_ORDERS`, `LOCKSTEP_UPLOAD_SEATS_ACTION`
  in `regia_net_soak::report`); beyond it the bar is skipped with the reason and the measured numbers. A custom run
  shorter than 5 minutes is judged on the seconds it has, and its detail gives the run length. A run with no whole
  measured second fails.
- The bars that read snapshots (`staleness_p99`) are skipped with `lockstep run: no snapshots`.

A failed bar names each violation. When every violation of a bar is a known blocker, the bar is tagged with the
branch that fixes it. Any other violation is a new finding. The known blockers at v0.5.0 are:

- `v051-net-input-mtu`: an input bundle bigger than the datagram makes every send fail (`too_large`), so the client's
  inputs stop reaching the server.
- `v051-net-input-rejection`: an over-cap input never gets its sender dropped.

## The gate row

`cargo xtask preflight` (and so the merge gate) runs `cargo xtask net-soak --plan gate` when
`xtask::net_soak::GATE_ROW_ENABLED` is `true`. The constant is `true` since the v0.7 freeze (ADR-10265: both blockers
merged, the gate plan read `61 runs, PASS`). How it was turned on:

1. Merge both blocker branches.
2. Run `cargo xtask net-soak --plan gate` on the build server and check that it is green.
3. Set the constant to `true` in a commit that cites that run.

`REGIA_NET_SOAK_GATE=1 cargo xtask preflight` runs the row once without the change, and `REGIA_NET_SOAK_GATE=0` skips
it once it is on. The build machines gate script runs the same command as its own step.

The gate set is every replication profile and `congested+bulk` at 8 clients, plus `bad-wifi`, `mobile-4g`, `flappy`,
`hostile` and the spec 120 replication profiles at 32 clients, and `mixed` at 40. `lockstep-mix` runs at 8, 32 and
40 seats, `lockstep-mix+action` at 16, 24 and 40, and `lockstep-mix+bots` at 40 seats for 600 s. It also runs `good-wifi` and `congested` at 8
clients on a listen server. That is 61 runs. Each run is 60 virtual seconds, or longer where a profile's schedule
needs it (`rtt-step` 180 s, `long-outage` 120 s, the orders and action lockstep runs 300 s: the length `lockstep_upload` is stated
over; the bots row is 600 s); `cgnat` runs at least 10 clients. The nightly set is every profile at 8, 32, 64 and 128
clients in both server modes, 300 virtual seconds each (or more, as above); the lockstep profiles run at up to 40
seats there (8, 32 and 40; the action mix also at 16 and 24), not at 64 or 128. The bots mix at 40 seats is 600 s
in the nightly plan too.

## Two hosts

`cargo xtask net-soak --two-host HOST_A HOST_B` prints a recipe and exits. It does not open a socket and it does
not ssh. `HOST_A` runs the host process. `HOST_B` runs one `lockstep_bot seat` process per seat. The host binds
`0.0.0.0` (not the loopback address) and the seats dial `HOST_A`'s address, so the datagrams are real UDP between
the two machines. `--two-host-seats` (default 4), `--two-host-ticks` (default 600) and `--two-host-port` (default
7788) are the numbers the recipe prints. The seats use `--mix bots --seed 24301` (`0x5eed`, the bot's default seed).

Pass when all of these hold:

- the host prints `verdict PASS` and `desyncs=0`;
- every seat process exits 0;
- the host prints an `upload` line (`payload_bytes`, `datagrams`, `wire_bytes`, `elapsed_ms`, `wire_bps`, `desyncs`).

`verdict FAIL`, a non-zero seat exit, or a missing upload line is a failure. The process still exits 0 when the
match finishes with `verdict FAIL`: the verdict line is the pass signal, and exit 2 is only the 90 s deadline.
A short proof is 4 seats and a few hundred ticks. The 40-seat, 600 s virtual run stays in
`lockstep_mix_40_seats_desync_zero_10_min` (`cargo test -p regia_net_soak --test lockstep lockstep_mix_40_seats_desync_zero_10_min -- --ignored --exact`).

## Reproducing a failure

Every failed bar in `summary.md` ends with its command, for example
`cargo xtask net-soak --profiles hostile --clients 8 --modes dedicated --seed 1592590337 --seconds 60`. The same
seed replays the same drops, holds and arrival order, so the failure repeats exactly. Only the wall-clock tick times
differ.

## Using it in a test

<!-- ignore-reason: uses the dev-only crate regia_net_soak, which the book's fence manifest does not link -->
```rust,ignore
use regia_net_soak::{profiles::Profile, report, workload};

let s = workload::Scenario {
    profile: Profile::named("bad-wifi").expect("profile"),
    clients: 8,
    mode: workload::ServerMode::Dedicated,
    seed: 42,
    duration_ms: 30_000,
};
let result = report::evaluate(&workload::run(&s)?, 0);
assert!(result.passed, "{}", result.bars.iter().map(|b| &b.detail).cloned().collect::<Vec<_>>().join("\n"));
```

To show a bar catching a broken case, run the scenario with `workload::run_with(&s, &knobs)`. Besides the
timeout policy, the input lead, the relay attacker and the rate controller, `Knobs::silent_inputs` (one client
sends no input) and `Knobs::silent_snapshots` (the server sends one client no snapshot) take a
`workload::Silence { client, from_ms, until_ms }`; both must fail `no_wedged_client` (ADR-8887,
`crates/regia_net_soak/tests/wedge_hole.rs`).

Your own game's netcode can use the impairment network on its own: `ImpairedNet::new(seed)`, then
`add_endpoint(addr, LinkSpec { .. })` per peer and `socket(ep)` for a `DatagramSocket`. Call `advance_to_ms(now)`
before you update the endpoints.

## The time harness

`regia_net_soak::timed::TimedHub` (ADR-8185) runs replication and client prediction, not just raw messages, on the
impairment network. Each process is an ECS world with `regia_net`'s fixed tick (receive, apply, your gameplay,
predict, send) behind a `NetLink`, so:

- reliable frames arrive only by the transport's own acknowledgments and resends;
- prediction reads its round trip from the transport's RTT estimator, the way a game does (no `set_transport_rtt`);
- every process ticks on its own clock: a link's `clock_ppm` of `5_000` makes that client tick, and read its
  milliseconds, 0.5 % fast;
- clients start at a seeded phase inside the server's tick and datagrams arrive at their microsecond
  (`ImpairedNet::advance_to_us`), so frames land between ticks;
- RTT steps are link phases (`rtt-step`), and `timed::rtt_ramp(base, from_ms, to_ms, start_ms, len_ms, steps)` builds
  a ramp.

The same seed gives the same run. `TimedHub::report(client)` returns a `TimeReport`, counted from 2 s after the
client connects (`TimedOptions::warmup_ms`):

| Field | Meaning |
|---|---|
| `corrections`, `resets`, `resyncs`, `dilated` | the client's `PredictionStats` over the window |
| `backward_steps` | client updates where the predicted tick went backward (spec 120 § 9.1) |
| `missing_inputs`, `late_inputs` | server ticks run on a repeated input; inputs that came after their tick |
| `lead_error_ticks` | per server update with a newer input: ticks early it arrived minus the client's margin target (`samples`, `mean`, `p50`, `p95_abs`, `max_abs`) |
| `lead_late_samples` | of those, how many arrived after their tick |
| `rtt_error_ms`, `rtt_settles` | the transport's RTT estimate minus the path's round trip; per path change, ms until it was within 20 ms + 10 % |
| `reliable_resends`, `disconnects` | the client transport's resends; times its link ended |
| `correction_m`, `snaps` | the pose change of each correction or reset (`PredictionStats::last_correction_m`; spread as above); rollbacks whose rendered pose jumped (ADR-8219) |

`TimedHub::lead_samples(client)` gives the lead samples behind `lead_error_ticks` with their time: `(ms since the
connect, arrival margin, margin target)`, for startup checks (spec 120 § 9.2).

`TimeReport::line()` prints them on one line. The harness judges none of them: the prediction, clock and lead lanes
set their bars on these numbers. `cargo test -p regia_net_soak --test time_harness -- --nocapture` runs the
10-minute `rtt-step`, `clock-drift` and combined sessions, a 2-minute `high-rtt` session and a parity run of
`regia_net`'s `no_false_corrections_under_latency_jitter_and_loss`, and prints each client's line. Two sessions hold
spec 120 bars (ADR-8219): `high_rtt_prediction_stays_on_for_five_minutes` (450 and 650 ms RTT: at most one reset,
correction p95 within transatlantic + 20 %, no snap) and `startup_lead_on_transatlantic_is_right_within_two_seconds`.

<!-- ignore-reason: uses the dev-only crate regia_net_soak, which the book's fence manifest does not link -->
```rust,ignore
use regia_net_soak::profiles::Profile;
use regia_net_soak::timed::{TimedHub, TimedOptions};

let mut hub = TimedHub::new::<MyGame>(TimedOptions { seed: 7, ..TimedOptions::default() }, setup)?;
let c = hub.add_client::<MyGame>(Profile::named("rtt-step").expect("profile").client_link(0, 600_000))?;
hub.client(c).gameplay.add_systems(feed_inputs);
hub.run_until(10_000, |h| h.client_peer(c).is_some());
// ... spawn the client's predicted pawn on hub.server().world ...
hub.run_ms(600_000);
println!("{}", hub.report(c).line());
```

## Client captures of a failing run

A run that fails its bars is run again (the soak is deterministic from its seed) with a client capture on every honest client
(`regia_net::capture`, ADR-8221), and each client's capture is written to
`<REGIA_NET_SOAK_CAPTURE_DIR or REGIA_NET_SOAK_OUT/captures>/<profile>-<clients>-<mode>-seed<seed>/client-<i>.netcapture`, with
its replay command on stderr:

```text
net-soak: bad-wifi/8/dedicated client 3 capture …/bad-wifi-8-dedicated-seed1592590337/client-3.netcapture
  replay: REGIA_NET_SOAK_REPLAY=…/client-3.netcapture cargo test -p regia_net_soak --test capture_replay -- --ignored --exact replay_capture_file --nocapture
```

The replay rebuilds the client from the scenario in the file's label, feeds its connection the captured decrypted packets at the
captured client milliseconds and runs its frames at the captured frame times. It prints the client's corrections, inputs and output
digest, and the first record where the replay differs from the capture (a frame digest, an input stamp, a receive or flush time, an
end); it passes only when nothing differs. Each client's ring is `capture_ring_bytes` (96 KiB per virtual second, 8-128 MiB); a
client whose ring lost its first session says so and cannot replay from the start. With neither variable set, the failing run
prints how to get its captures. `workload::run_with_captures(scenario, &taps)` and `workload::replay_client(scenario, i, &file)`
do the same from a test.

Error codes of `regia_net::capture` (lowercase, in `CaptureError::code`): `capture_config_invalid` (ring outside 4 KiB..=1 GiB),
`capture_io`, `capture_exists`, `capture_not_a_capture`, `capture_version`, `capture_truncated`, `capture_corrupt`,
`capture_empty` (no whole session), `capture_protocol_mismatch`.

## Limits

- The network is in-process. It tests `regia_net` from the socket seam up, not the operating system's UDP stack. The
  stack's own behavior (buffer sizes, ICMP) has its own tests in `regia_net` (`socket.rs`) and the
  [net match](net-match.md) across real machines.
- The sample game sends raw `NetServer`/`NetClient` messages and inputs. Replication, relevance, prediction and
  interpolation of spec 70-74 run on top of the same transport and have their own conditioned tests.
- The time harness runs one fixed tick per update (no render frames between ticks) at an integer 16,667 us period
  for every process; only `clock_ppm` moves processes against each other.
- Server tick times are host-dependent. Only `tick_p99` (non-gating, on `mixed`) judges them; judge it one run at a
  time on a quiet host (`--jobs 1`), since parallel runs and a loaded host move the wall clock.
- The real UDP wire uses real sockets in one process: it measures the system calls and kernel copies of the server's
  traffic, not a second machine's network card or a remote client process (spec 120 § 15.3's two-machine release
  check).
- `relay-path` is declared but skipped until relay v2 exists.
- `lockstep-mix` runs through the plan since ADR-8209 ([Lockstep runs](#lockstep-runs)). `desync_zero` judges the
  no-desync half on it; the injected desync that must be named and resynced is in
  `cargo test -p regia_net_soak --features desync-inject --test lockstep`
  ([Net lockstep](net-lockstep.md#tests-and-the-soak)). The input mixes are the soak's own scripts (`InputMix::ORDERS`,
  `InputMix::ACTION`, `InputMix::WANDER_FIRE` as `lockstep-mix+bots`), not a recording of real matches. The two-host
  recipe is [Two hosts](#two-hosts).
- The soak's failure captures replay its raw `NetClient` clients. An ECS world (prediction and interpolation of spec
  73-74) replays through `NetLink::replay`, and `net.capture_start` / `net.capture_save` capture a running client: see
  [Net client capture (agents)](net-capture.md) and the time harness's `TimedHub::capture_client` and
  `timed::replay_client` (ADR-8890).
