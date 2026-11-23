# Net match across machines (agents)

`cargo xtask net-match` plays the `net-game` template across real machines over UDP: a dedicated
server on one host, windowed and headless clients on others, every process with the same stated
network conditions (ADR-3075). It reports match length, reconciliation counts, bandwidth and
whether every process agreed on the final state. The in-process and loopback tests stay the
regression net; this command shows that the template works on a real network.

## Stated conditions: conditioner specs

Every outbound datagram of a `NetLink` server or client passes through its seeded conditioner
(`regia_net::conditioner`). It shapes the real UDP socket, not a simulation. A **conditioner
spec** is one line that states the conditions:

| Spec | Meaning |
|---|---|
| `off` (or empty) | Pass-through |
| `home`, `wan`, `wan_stress`, `bad`, `burst`, `lan` | The link profiles |
| `good`, `average`, `terrible` | The session presets (spec 77 § 10.1), mapped onto the link with the same latency range |
| `delay=40,jitter=10,loss=0.02` | Stated conditions on top of `off` |
| `wan,loss=5%,seed=7` | A profile with overrides |

Keys: `delay`, `jitter`, `reorder_max`, `burst_start`, `burst_len` (milliseconds, at most 60 000);
`loss`, `dup`, `reorder`, `burst_loss` (fractions in `[0, 1]`, or percent with `%`); `seed`. The
jitter is an extra hold in `0..=jitter` on top of `delay`. `bad` is a link profile and a session
preset; in a spec it means the link profile.

Where a spec is accepted:

- `net_game_client --conditioner SPEC [--seed N]` and `net_game_server --conditioner SPEC
  [--seed N]` (the template's binaries, every build).
- `REGIA_NET_CONDITIONER` and `REGIA_NET_CONDITIONER_SEED` in **dev builds only**: the template's
  hosted server (`hosted_conditioner`) and client read them (play-in-editor passes them too). A
  build without debug assertions (`release`, `shipping`) ignores them, so a shipped game cannot be
  given loss from the environment.
- `net.conditioner_set` with `spec` (see below).

Each process prints `conditioner <conditions>` at start, for example
`conditioner delay=60 jitter=10 loss=0.020 dup=0.010 reorder=0.050 reorder_max=40 seed=3`.

## Agent method

`net.conditioner_set` takes `spec` as an alternative to `profile`. `config` still overrides fields
afterwards. The result adds `conditions`, the same one line:

```json
{"method": "net.conditioner_set", "params": {"endpoint": "client", "spec": "delay=40,jitter=10,loss=2%"}}
→ {"queued_for": ["client"], "conditioner": {...}, "conditions": "delay=40 jitter=10 loss=0.020 dup=0.000 reorder=0.000 reorder_max=0 seed=1"}
```

| Code | Meaning |
|---|---|
| `net_bad_conditioner` | The spec does not parse (the message names the part), or `spec` and `profile` were both given |
| `F0002` | `spec` is not a string |

The other codes are those of [Net link (agents)](net-link.md) and [Networking (agents)](net.md).

## The `netstats` line

At the end of a run, the template's client (headless, or windowed with `--ticks N`) and server
print one line from `net_game::net_stats_line`. It has `key=value` words: `who`, `role`, `state`,
`tick`, `peers`, the endpoint totals (`datagrams_in`, `datagrams_out`, `malformed`,
`auth_failed`, `replayed`, `socket_errors`), the conditioner counters (`cond_passed`,
`cond_dropped`, `cond_duplicated`, `cond_delayed`) and `cond`. A client adds its link counters
(`rtt_ms`, `jitter_ms`, `loss`, `packets_*`, `bytes_sent`, `bytes_received`, `resends`,
`duplicates`) and its prediction counters (`corrections`, `max_error_m`, `resets`, `resyncs`,
`teleports`, `verify_failures`). A hosted server prints it from its stop hook, using the status
the host kept when it closed the game port (`HostedLink::final_status`).

The server also reports `shots`, `hits` and `rewound` from the game's authoritative
`ShotLog`. Preserve these with the final scores: process acceptance requires nonzero
rewound shots and a hit count matching the sum of awarded tags.

## Running a match

1. On every build host, generate the project against that host's engine checkout and build it in
   the dev profile. Build the server and the client separately:

   ```sh
   cargo xtask net-match generate --out /work/netmatch --engine /work/regia
   cd /work/netmatch && cargo build -p netmatch_server && cargo build -p netmatch_client
   ```

   Two cargo invocations, not one: cargo unifies features across every package of one build, so
   `cargo build -p netmatch_server -p netmatch_client` (or `--workspace`) gives the client
   `regia`'s `server` feature. Such a windowed client refuses to start and says so
   (`regia::SERVER_BUILD`); a headless one still runs.

   The generated project copies the engine's `Cargo.lock`, so every host builds the same
   dependency versions. A release server ignores `REGIA_NET_CONDITIONER`, and the run refuses it
   when a conditioner was asked for.

   The generated dev profile optimizes dependencies at level 2, as the quick template does.
   Game crates remain debuggable, and debug assertions keep development conditioning enabled.
   Record the profile and cold/warm startup timing: an unoptimized renderer that falls behind
   the fixed-step rate can agree after settling while still failing reconciliation bounds.

2. Run the match from any machine that can ssh to the hosts (`local` runs on this machine):

   ```sh
   cargo xtask net-match run \
     --server rig --server-bin /work/netmatch-target/debug/netmatch_server \
     --server-project /work/netmatch --server-dir /work/netmatch/srv --server-ip <lan-host> \
     --client windowed,local,/work/netmatch-target/debug/netmatch_client \
     --client headless,the build server,/srv/netmatch-target/debug/netmatch_client \
     --conditioner delay=40,jitter=10,loss=0.02 --seed 11 --ticks 3600 --settle 600 \
     --out target/net-match/run1
   ```

   The server host must accept inbound UDP on `--port` from the client hosts. A host whose
   firewall drops it cannot be the server: every client ends with `connect_timeout`. Check with a
   one-datagram echo between the hosts before the first match. Clients need no inbound rule (the
   server answers from the port they sent to).

   `--dry-run` prints the `server.toml` and every command without starting anything. A client
   binary that ends in `.exe` is started through the Windows shell.

3. Read `report.md` (and `report.json`) in `--out`. There is one row per client: whether it agrees
   with the server, the largest position error, RTT, jitter, loss, kbit/s up and down, resends,
   corrections, resets, resyncs and verify failures. The raw outputs are `server.txt` and
   `client<i>-<kind>.txt`.

How it judges: each client's bot plays `ticks - settle` fixed steps and then stands still. The
server stops (console `quit 0`) only after every client has finished, and prints a `summary` line
per avatar still in the game. A client that finished earlier has left by then; the server printed
its avatar's final state when it removed it, as a `left slot=… x=… z=… tags=… tagged=…` line. Every
client must have seen every `summary` avatar, and every avatar it saw (a `left` one included) must
have the server's score and be within 2.1 mm of the server's position (print rounding plus the
prediction tolerance). A client that finished later may no longer see a `left` avatar. Anything
else is a desync, and the command exits 1. Start offsets between clients must stay below
`--settle` ticks, so every bot stands still before the first client prints what it saw.

## What still needs the owner's network

Every run so far crossed one LAN segment with no NAT. A match from the internet needs one of these:
the UPnP or PCP/NAT-PMP port mapping (`regia_net::upnp`, `port_map`), which only a real home router
can answer; a forwarded UDP port; or the relay. The run cannot prove any of these until the owner's
router allows them.

## Acceptance checks and limits

Run the scoped checks on the candidate whose binaries will be used in the match:

```sh
cargo test -p regia_net --test prediction --test lagcomp_input_stream --test transport_trust --test server_host_e2e
cargo test -p net_game --test e2e --test process_pair --test agent_smoke
cargo test -p net_game --features server --test client_build_mode
cargo test -p regia_cli --test server_package_e2e -- --ignored --test-threads=1
```

The build-mode test intentionally enables both modes and requires the windowed binary to exit
with code 2 before opening a window. Rebuild the real client separately afterwards.
`server_host_e2e` checks content/protocol mismatch refusal, a truncated manifest delivered through
an encrypted conditioned UDP link, and suspend/resume with the same reserved player slot and a
rotated reconnect token. These are real UDP checks on one host; they do not certify a packaged
template's content handshake or a two-host match.

The ignored `server_package_e2e` layout/runtime test also loads the manifest from the unpacked
dedicated-server sample and connects real encrypted UDP clients to that executable. It requires
`content_mismatch` and `protocol_mismatch` refusals, then a successful matching client and a
same-slot reconnect with a rotated token. This is packaged session-host evidence; the replication
template's two-host match is a separate run.

`agent_smoke` runs a server and client over real UDP, waits for the replicated avatar with
`play.wait_until`, asserts its spawn position with `play.expect`, and sends
`net.conditioner_set` with an explicit `spec` and seed. It checks the seed reported by `net.status`.
It needs no GPU. `render_smoke` separately captures both avatars through `seeing.screenshot`.

Keep the raw match logs alongside the report. Record every endpoint's seed, nonzero impairment
counters, prediction corrections/resets/resyncs and maximum error, and the final position/score
comparison. A same-host test, a cross-compile, and an authenticated Steam session are different
evidence: the Windows match must actually execute on Windows, and Steam authentication has its
own owner-operated harness. An ignored package test counts only when explicitly executed.

The final-state judge requires a successful client exit and at least one complete, finite avatar
record. Every reported slot must exist in the server's current or departed final state. Missing
fields, duplicate slots, unknown slots, NaN and infinity cannot establish agreement. JSON client
rows expose `judged` separately from `connected` and `agrees`. Client conditioner seeds use wrapping
u64 addition, matching the template even when the server seed is `18446744073709551615`.
The aggregate report also requires one authoritative final avatar per client and complete server
records. It includes `server.final_avatars` (current plus departed slots), and a completed run adds
`server.exited_ok`. A server process failure makes the saved report fail as well as the command.

## Dedicated app composition

The dedicated app includes both `NetSession` and the template's replication `NetLink`.
A peer admitted by the live server link keeps the replication input path (ADR-4861).
Other peers still need a declared player slot when a session tracks them. An idle link
or a client link does not authorize server input. This avoids rejecting every valid
replication input merely because an unrelated session exists in the same app.

`cargo test -p net_game --features server --test client_build_mode` also compares late
UDP joins into `regia::server::build_app` and the minimal server app. It advances the
server 600 ticks before connecting, then requires movement, exact settled agreement,
bounded corrections and zero verification failures. The normal windowed binary must
still be rebuilt separately after this deliberately unified test build. Use
`net.prediction`, `scene.query_entities` and `play.expect` to inspect the same behavior
in a running game; preserve the final `netstats` counters with the match logs.

## Windowed acceptance on a virtual display

Keep presentation settings and startup timing with the raw logs. On the November 9
Linux LAN run, a 1280×720 Xvfb surface spent roughly 50–62 ms per frame presenting.
The final positions agreed, but reconciliation exceeded the sample's original
limits: at most 20 corrections and two resyncs per client, with zero verification
failures. Agreement alone does not satisfy those limits, and startup counts.

The existing agent surface can prepare a smaller window without changing the
simulation or conditioner: query `scene.query_entities` with `component: "Window"`,
then use `scene.set_property` on that returned entity to set `Window.logical_size`
to `[640.0, 360.0]` and `Window.present_mode` to `"Immediate"`. Confirm the actual
size and effective mode through `render.swapchain`. Apply this when the endpoint
becomes ready, before judging the match; retain the change responses and do not
reset prediction counters. Capture the window with `seeing.screenshot`, and use
`play.wait_until` / `play.expect` on the replicated avatar's Transform.

That controlled window passed the 25 ms delay / 10 ms jitter / 2% loss / 1%
duplication / 2% reorder profile with seeds 11/12/13. The 40 ms delay / 10 ms jitter /
2% loss / 5% reorder profile with seeds 23/24/25 first exceeded the resync limit
(+12, +9, -9 in the first second). The cause was the lead clock, not the window
(ADR-4915): prediction started before the transport had sampled its round trip,
taking `rtt_ms` 0 as a 0 ms round trip (lead 2 at about 130 ms), and after the
first jump the stale part of the server's averaged margin counted as a new error
and a new negative margin. With the fix, a client logs
`net.prediction_started` with a lead near `ceil(rtt / step) + margin_target` and no
`net.lead_resync` in a steady match; a lead resync at startup is now a finding to
investigate, not noise. Repeat both profiles after any change to `predict.rs` or
the link, and keep the raw logs. The test results and artifact hashes are recorded
in the sprint progress/report, not a renderer performance certification.

A Windows client host whose ssh shell is Git-Bash (not `cmd`) needs the client
binary with forward slashes, `headless,user@host,C:/dir/netmatch_client.exe`: the
harness quotes `.exe` paths for `cmd`, and bash drops the backslashes of
`C:\dir\...` (`command not found` in `client<i>-headless.txt`). A client started
over ssh on Windows has no desktop, so run it `headless`; a visible Windows window
needs a console-session launch (`schtasks /it`) outside the harness.

A cold first frame on a software renderer (18.8 s under lavapipe, 25.4 s in a two-machine
match) outlives the server's handshake state: its cookie is honored for one to two 10 s
buckets and a half-open key exchange for 10 s. A client whose answer relies on state that may
have expired starts a new attempt with a fresh nonce (at most twice per connect, ADR-4929) and
logs `handshake state may have expired on the server; starting over`. In `net.status` the
symptom of the old behavior was `disconnected:connect_timeout` with `totals.datagrams_out` 1 and
`datagrams_in` 1 (the server answered; the client gave up). A windowed client that cannot bind a
local UDP port now exits with status 3 at once instead of running disconnected.

## Loopback acceptance (N01 and N03)

`regia_net`'s `tests/net_acceptance.rs` is the in-process acceptance of the netcode core on the
loopback hub at fixed steps, over a conditioned link, seeded, with no wall clock:

```sh
cargo test -p regia_net --test net_acceptance -- --nocapture
```

- `n01_..._on_the_reliable_stream` and `n01_..._on_the_state_channel` (negotiated, the state
  link must end `Acked`) run one session with two clients at 10 % loss, 2 ticks of delay, 2 %
  duplicates and 5 % reordering each way. A game component with a derived codec and one with a
  hand-written codec arrive exactly; a one-field change after the spawn travels in frames under an
  eighth of the spawn frame's size; spatial relevance gives each client only what is within its
  radius and follows a move both ways; twenty reliable to-server calls arrive once each in order,
  the one `validate` rejects is counted and never runs; an entity broadcast reaches only the client
  that holds the entity, a session broadcast reaches both once, and an unreliable sequenced
  to-owner stream reaches only the owner, never out of order. The same seed repeats the session.
- `n03_two_shooters_on_a_lossy_link_hit_where_they_saw_the_target`: two clients at 4 ticks each
  way with 5 % loss, duplicates and reordering fire 100 shots each, through the input stream, at
  a target strafing at 6 m/s. At least 95 of each client's shots are judged (a command lost with
  all its redundant copies is never judged), and every judged shot hits `body`. Each shot is posed
  at the view tick its command carried (the client's interpolation tick when it captured the fired
  tick, as quantized on the wire), clamped to the rewind window and the recorded history (spec 75
  § 5 step 1). At this link's delay the carried view is older than the default 250 ms window, so
  the clamp poses every shot at the window's edge. A second run with a 400 ms window poses every
  shot exactly at its carried view tick. With rewind off, both clients still have at least 95
  shots judged, all at the present tick, and at least 90 % of them miss. The same seed repeats.

The lag-compensation details of spec 75 § 13 (fractional ticks, clamps, occlusion, teleport cuts,
dead targets, joints, claims, projectiles, read-only queries) are in `tests/lag_compensation.rs`
and `tests/lagcomp_input_stream.rs`; replication, relevance and the state channel each have their
own suite (`replication.rs`, `relevance.rs`, `state_channel.rs`, `derive_net_message.rs`).
