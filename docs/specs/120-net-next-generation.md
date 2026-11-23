# 120 — Netcode next generation: transport v2, lockstep and desync, resume, migration, quality and soak bars

Status: design (ADR-7905, proposed). Written 2026-11-15 by the net-next-design lane at v0.5.0 `59ff4b61d`. No engine code
changes with this spec; the build lanes of § 17 implement it, each with acceptance tests that fail on its base.

Inputs (all read for this design): the netcode state-of-the-art audit (35 rows, experiments E1-E6), the DO27 netcode study
(37-row matrix, foundation verdict, DO27 provenance classes P1-P6), the net-soak harness results at v0.5.0 (findings F1-F7),
the adversarial review (17 rows), and the two v0.5.1 fix lanes (ADR-7892 hostile input rejection; ADR-7896 size-aware input
bundles and the reliable input lane, wire version 3). Those reports live in the build machines's scratch reports; the facts this spec
relies on are restated where they are used, with `crates/regia_net/src/` file:line citations at `59ff4b61d`.

How to read the provenance marks. Every design element carries one of:

- **[PUB]** cleanroom-derived from a cited public source (RFC, paper, public talk). Original code.
- **[D27]** follows a design the owner wrote for Direct Order 27 (DO27), adopted as a requirement and test scenario, re-derived
  in Regia (the ADR-1600 method: designs and behavior, not code). Code may be ported only from DO27 files of class P3 (written in
  DO27's repo after 2026-10-01 under its cleanroom rule), each with a `docs/provenance.md` row naming the DO27 crate, file and
  commit (§ 2.3).
- **[REG]** exists in `regia_net` today and is kept, possibly fixed.
- **[NEW]** a Regia design decision with no single source; the reasoning is in this spec and ADR-7905.

## 1. Goal, promise and scope

The owner's goal (2026-11-15): improve drastically on DO27's netcode and on Regia's own, and build the best netcode we can, with
the features that keep a game from lagging out or dropping players. This spec turns that into one architecture and into
measurable promises. "Better" is defined row by row in § 16 against both today's `regia_net` and DO27's live `int/v020` path, and
every promise below is a soak bar (§ 15) that runs in the gate.

### 1.1 The network envelope we promise (proposed; owner question Q4)

| Promise | Bar | Profile(s) |
|---|---|---|
| Playable: prediction on, lag compensation exact, no hitch from the netcode | up to **650 ms RTT, 5 % loss, 60 ms jitter** | high-rtt, bad-wifi, mobile-4g |
| Never dropped by a short outage | an outage shorter than the session timeout (default **20 s**) ends with the player still in the match, a visible `Interrupted` state during it, and every reliable message delivered once, in order | flappy, long-outage |
| Never lose your slot | a player that returns within the resume grace (default **60 s**) is the same `PeerId` with its entities, in every game kind (session, replication, lockstep) | long-outage, wifi-to-cell |
| Never dropped for ping or loss | a peer at 3,000 ms RTT and 20 % loss is never removed; removal is for silence (or a policy the game chose) only | rtt-extreme |
| Address changes are invisible | a NAT rebinding, a Wi-Fi to cellular switch and nine players behind one CGNAT address keep playing | nat-rebind, wifi-to-cell, cgnat |
| The sender never hurts the path | the offered rate settles at or under the bottleneck within 5 s; queueing delay p95 <= 100 ms on a 1 s drop-tail queue | congested, bufferbloat |
| A big transfer never freezes gameplay | small RPC p95 within 1.5x the no-transfer control while a 256 KiB transfer is in flight on a capped link | congested + bulk |
| Inputs arrive in time | missing inputs at the server <= 0.5 % on good-wifi, <= 2 % on bad-wifi and mobile-4g, <= 3 % under downstream bufferbloat | good-wifi, bad-wifi, mobile-4g, asym-bufferbloat |
| One hostile peer harms nobody else | each hostile session dropped within 2 s of its first act; others' input arrival and staleness within their own control values | hostile, flood |
| It scales | 64 clients on a dedicated server on the mixed bad-network profile (gate-sized at 40), 128 recorded; a listen server holds 16 on the minimum-spec Windows laptop and 32 on a 20 Mbit/s uplink | mixed, listen-uplink |

**Built (N215, ADR-8454).** "It scales" runs as `cargo xtask net-soak --plan scale` / `--plan scale-gate` (§ 15.2 Built
note): at the lane's base 40 and 64 clients on `mixed` hold every player within their tick tiers, 128 hold every player
but miss theirs, the listen server holds 16 on the min-spec Windows laptop and 32 on `listen-uplink`; numbers in
`docs/perf/net-scale-soak.md`.

**Built (v07-net-scale-128, ADR-10081).** Re-measured on the v0.6.0 tag plus the v0.6.1 net lanes, one run at a time
on one quiet the test machine core: 128 clients meet their tier (tick p99 3.0-5.2 ms against 16.666 ms; the 30 ms above was the
loaded measurement host). The server tick still grew faster than the client count, because the loss detector re-read
each connection's whole sent-packet history on every poll and acknowledgment; it now starts at the oldest packet
without a verdict, and the per-poll scratch left the heap: tick p99 2.0-2.7 ms at 128, 0.7-0.8 ms at 64, 0.4-0.5 ms at
40, server CPU 4.3-5.4 % of a core at 128 (0.035 % per seat; 0.023 % at 40). Tables in `docs/perf/net-scale-soak.md`.

### 1.2 Scope

In: every row of § 16 (37 rows), the three DO27 adoption needs held for this design (a relay for many clients, the net core
without the ECS/app stack, a rollback log for a game-owned step), DO27's full move onto `regia_net` (§ 14), and compatibility
with v0.5.x hosts (§ 12).

Out, with the reason:

- **Peer-to-peer mesh topologies** (every peer sends to every peer). Star topology (server, listen server, or lockstep host)
  covers every game the engine targets; a mesh multiplies NAT traversal and trust work for no game we know of. A lockstep game
  is a star with a host that bundles frames (§ 7).
- **Browser transport (WebRTC data channels / WebTransport).** The web player has no multiplayer in its scope today. The sans-IO
  transport (§ 3) keeps the port possible: a browser `DatagramSocket` would sit under the same layers.
- **QUIC as the main transport.** `quic.rs` stays a loopback/LAN option (it is loopback-only, quic.rs:3). Transport v2 takes the
  parts of QUIC's design a game needs (packet numbers, ack ranges, RFC 9002 loss detection, streams, path validation) on top of the
  existing Noise NX record layer, which already handles encryption, the stateless handshake and rebinding. Two edges of the QUIC
  option are fixed anyway (§ 11.6).
- **Kernel-level anti-cheat and client attestation.** The authority model (spec 70 § 5) and the hostile-input policy (§ 11) are the
  engine's anti-cheat; client integrity checks are a store or platform service.
- **Interest management that hides state from a lockstep client.** Inherent to lockstep (every seat runs the whole sim). Games
  that need wallhack resistance use replication (§ 8), which filters per client.

## 2. Foundation: what comes from where

### 2.1 Decision

**The next `regia_net` is built on today's `regia_net`.** DO27's lockstep, desync, migration, timeout, version-probe,
evidence and quantization designs and their test scenarios are merged into it as designs ([D27]). DO27's `ir-net` transport,
server and relay code is **not** ported. Code from DO27 may be ported only from class-P3 files, file by file, where the code is
generic and better than a re-derivation, each with a provenance row (§ 2.3).

Why (the study's evidence, re-weighed for this design):

1. **The faults are of different kinds.** Regia's 15 WRONG rows are bugs in the right architecture (an ack header that cannot
   name a burst, a timeout not wired, a dilation that skips a capture); each has a fix lane with a failing test. DO27's weak rows
   are architectural: its live path puts every input, frame and checksum on one reliable-ordered stream with a fixed 250 ms
   resend, so one lost packet stalls every seat, and its transport runs in a mode whose token key is all zeros, so any observer
   can read and forge traffic.
2. **Security cannot be retrofitted cheaply.** DO27's transport would need an authenticated token channel it does not have;
   Regia's Noise NX transport, stateless cookie, replay window and sealed rebind already exist and are tested under impairment.
3. **Coupling.** DO27's protocol module imports its sim's types; its server is its game. Regia's `NetGame` traits carry no game
   data.
4. **What DO27 does better is mostly design and practice**: lockstep with adaptive delay and agreed-tick changes, desync
   detection down to the subsystem, live host migration, a timeout policy with load grace, a version probe, a real-process soak,
   measured 40-seat numbers, quantized records with distance tiers, operator telemetry. All of these are adopted below.

### 2.2 Part by part

| Part | Comes from | Mark | Section |
|---|---|---|---|
| Sans-IO `Connection`, endpoints, `DatagramSocket` seam, deterministic ms time | Regia, kept | [REG] | 3 |
| Noise NX record layer, replay window, stateless cookie, per-source limits, sealed rebind | Regia, kept and hardened (path validation) | [REG]+[PUB] | 3.1, 10.4, 11 |
| Packet numbers, ack ranges with ack delay, loss detection and probe timeout | RFC 9000 § 12-13, § 19.3; RFC 9002 § 5-6 | [PUB] | 4.1 |
| Rate controller and pacing | GCC (draft-ietf-rmcat-gcc), BBR (Cardwell et al. 2016), RFC 8085, RFC 9002 § 7, RFC 8083 | [PUB]+[NEW] | 4.2 |
| Bounded, loss-driven back-off whose recovery is never slower than the cut; per-tick (never per-frame) I/O budgets; no duplicate datagrams; reorder is not loss; socket buffers read back | DO27 anti-pattern rules A1, A3, A4, A6, A7, A8 | [D27] | 4.2, 4.5, 15.4 |
| Multiple reliable streams, a real unordered class, per-stream reassembly caps and deadlines | RFC 9000 § 2-4; Fiedler 2016 | [PUB] | 5 |
| Size-aware input bundles, reliable input lane | Regia ADR-7896 | [REG] | 6.1 |
| Unacked-only input redundancy with delta coding; server input-margin feedback | Ford, GDC 2017 (public talk); Fiedler 2014 | [PUB]+[REG] | 6.1, 9.2 |
| "Never lose a press": a late input merges into the next unbuilt tick, presses and commands kept, newest analog wins | DO27 lockstep host | [D27] | 6.3 |
| Lockstep mode: host frames, adaptive input delay (raise at once with gap fill, lower after a calm window, switch on an agreed tick), silent seat, agreed-tick leave, start grace | DO27 live lockstep; Bettner and Terrano, GDC 2001; Fiedler 2014 | [D27]+[PUB] | 7 |
| Frames on the unreliable lane with redundancy, delay budgeted from jitter and loss (not RTT alone), equal delay for the host | Regia improvement over DO27 | [NEW] | 7.2-7.3 |
| Desync detection: periodic hash, per-subsystem probe, first-divergence replay, injection hook | DO27 | [D27] | 7.6 |
| Corroborated desync verdict and resync instead of match end | Regia improvement over DO27 | [NEW] | 7.6 |
| Acked-baseline deltas, priority accumulators, byte budgets, grid relevance with hysteresis | Regia, kept and fixed | [REG] | 8 |
| Quantized transform records, error-gated deltas, distance rate tiers, combat-heat priority input | DO27 state-sync design; Fiedler 2015 | [D27]+[PUB] | 8.2-8.4 |
| Clock filter, adaptive lead, fractional dilation, per-frame input capture | RFC 5905 § 10; Ford 2017 | [PUB] | 9 |
| `RollbackLog` for a game-owned step | GGPO-style rollback model (public documentation); Fiedler 2014 | [PUB] | 9.5 |
| Timeout policy: silence only, never ping or loss, configurable, start grace; stall edges 3 s / 8 s / 12 s as tests | DO27 rule A5 and its tests | [D27] | 10.1 |
| `Interrupted` state, resume for every game kind, slot reclaim, live-token takeover | RFC 9000 § 10; RFC 8446 § 2.2; Regia session tokens | [PUB]+[REG] | 10.2-10.3 |
| Path validation on rebind | RFC 9000 § 8.2, § 9 | [PUB] | 10.4 |
| Host migration: deterministic election, frame cache, resume at the highest tick, readout; successor journal for replication games | DO27 re-host and journal designs | [D27] | 10.5 |
| Authenticated election, best-connected winner | Regia improvement (DO27's own spec asks for best-connected) | [NEW]+[D27] | 10.5 |
| Relay with many clients per host (one allocation per client), permissions | RFC 8656 (TURN) § 5, § 9 | [PUB] | 10.6 |
| Candidate gathering and connectivity checks from the game socket | RFC 8445 (ICE), RFC 8489 (STUN), Ford, Srisuresh and Kegel, USENIX 2005 | [PUB] | 10.7 |
| Port mapping wired into hosting, port-forward self-check, host-capacity estimate | DO27 hosting flow; RFC 6886, RFC 6887 | [D27]+[PUB] | 10.7 |
| Plaintext version probe naming both versions | DO27 `IRVP` design; RFC 9000 § 6 | [D27]+[PUB] | 12.2 |
| Per-peer quality, grade, events; operator stats line with percentiles | RFC 3550 § 6.4.1; RFC 7679/7680; DO27 netstats design | [PUB]+[D27] | 13 |
| Real-process soak as a release check; 40-seat numbers recorded | DO27 practice | [D27] | 15.3 |
| Impairment harness, profiles, bars | Regia net-soak (ADR-7899); Gilbert-Elliott model | [REG]+[PUB] | 15 |

### 2.3 DO27 code that may be ported

Only from the study's class P3 (written in DO27's repo after 2026-10-01, full history, no denied-tree citation), and only where
the code is generic: `rehost_session.rs` and `live_rehost.rs` (lockstep re-host), `lockstep_migration.rs` (readout states),
`reconnect.rs` (catch-up from a snapshot plus a contiguous input log), `state_sync.rs` (quantization and tiers), `host_check.rs`,
`port_check.rs`, `netstats.rs`. Each port is a build-lane decision ("port or re-derive, and why") with a `docs/provenance.md`
row naming the DO27 crate, file and commit. Never from P2 (24 files from an earlier tree whose history is unrecorded) unless
the owner attests in writing to that tree's provenance (owner question Q7); never from P4 or P5; never anything from a
CLEANROOM-DENY path of either repo. DO27's renet glue is not ported (P6: nothing to gain). Readers follow
`docs/specs/25-sim-bridge-and-do27.md` § 2.

## 3. Layers and crate shape

### 3.1 Layer stack

```
game (NetGame traits, game-owned sim or ECS)
 |- lockstep (§ 7)        |- replication + prediction + interpolation + lag comp (§ 8, § 9)    [ECS layer, optional]
 |- session: join, resume, interrupted, migration, relay/NAT (§ 10)
 |- endpoints: NetServer / NetClient (peer table, input lanes, tick sync, quality) (§ 6, § 13)
 |- connection v2: packet numbers, acks, loss, rate controller, pacing, streams (§ 4, § 5)
 |- record layer: Noise NX sealing, replay window, rebind + path validation (kept) (§ 10.4, § 11)
 `- DatagramSocket: UDP, Steam, QUIC, relay, impairment (net-soak), loopback
```

Every layer stays sans-IO with millisecond time passed in, as today; nothing reads the wall clock or spawns a thread in the
transport. Tests and the soak harness drive it with virtual time.

### 3.2 The net core without the ECS (DO27 need "net-core-without-ecs") [NEW]

Today `crates/regia_net/Cargo.toml:24-27` links `regia_ecs`, `regia_app`, `regia_scene` and `regia_asset` unconditionally, and
about 30 files import `regia_ecs`, some only for a derive (`settings.rs:7` `Resource`, `model.rs:10`, `status.rs:11`) and some for
real ECS work (replication, prediction, interpolation, lag compensation, recorder, session plugin, server host).

Decision: **one crate, a default-on `ecs` feature** (not a crate split). With `--no-default-features` the crate builds the
core: `wire`, `bits`, `payload` (`NetGame`, `NetCodec`), `connection`, `crypto`, `handshake`, `server`, `client`, `tick`,
`congestion`/rate controller, `relay`, `join_code`, `port_map`, `upnp`, `discovery`, `snapshot_delta`, `lockstep`, `desync`,
`rollback`, `quality`, `known_hosts`, `password`, `rate_limit`, `addr_block`, `socket`, `conditioner`, and the session core
(`session/lifecycle.rs`, `proto.rs`, `bans.rs`, `manifest.rs`, `streams.rs`) without its plugin. Derives move behind
`#[cfg_attr(feature = "ecs", derive(Resource))]`; asset codecs (`codec.rs:182`, `:292`, `:1117`) move to an `ecs`-gated module;
`codec.rs:16` `Entity` codecs likewise. Reasons over a crate split: no public path breaks for hosts (a split re-exports but
changes `regia_net_core::` paths in docs, errors and API snapshots), one API snapshot, one provenance table. Cost: every
build lane must keep `cargo check -p regia_net --no-default-features` green; a gate check enforces it (§ 15.5) together with a
dependency assertion (no `regia_ecs` in `cargo tree -p regia_net --no-default-features -e normal`). DO27's headless server today
can already link `regia_net` + `regia_app` without a renderer; the feature removes the ECS and asset crates from that link.

**Built (net-core-no-ecs-a, ADR-8032).** As decided, with these refinements: `regia_platform` is optional too and rides `ecs`
(it links `regia_ecs` and `regia_app` itself; only the server host and the session plugin use it), and `voice` implies `ecs`.
`model`, `settings`, `status`, `lobby` (all but `lobby::plugin`) and the session core (`bans`, `conditioner`, `hub`,
`lifecycle`, `manifest`, `meter`, `proto`, `streams`) are core with their derives behind `cfg_attr`; `LobbySettings` moved out of
the lobby plugin into `lobby/settings.rs` (old path re-exported). `model`'s run conditions, `NetOwnerLink`, `resolve_owner`,
`OwnerCache` and `set_possession_owner` are `ecs`, as are `testing::LoopbackHub` (its `SplitMix64` stays core for the session
hub), the `.netprofile.ron` and `server.toml` fingerprints in `formats`, and `bridge`/`assets`/`profile_assets`/`runtime`/`send`
besides the modules named above. The asset and `Entity` codecs are gated item by item inside `codec.rs` rather than moved to a
new file (same effect, smaller diff for parallel lanes); without `ecs`, `CodecContext` carries only its counters. The core lib's
unit tests (352) pass with `--no-default-features`; integration tests, examples and benches still need `ecs`. The api-snapshot
scanner now lists a `cfg_attr` derive with its condition. Dependency assertion and headless example: net-core-no-ecs-b (N222).

**Built (net-core-no-ecs-b, ADR-8501).** The `Entity` and asset codecs, `EntityMap`, the asset-server context and the
`CodecContext` constructors that take them now live in the private `ecs`-gated module `codec/ecs.rs`, re-exported at their
`codec::` paths. `message` is core: the wire trait, direction markers, `Verdict`, `NetMessageRegistry` (plus new `decode` and
`validate` for a host that frames messages itself) and `#[derive(NetMessage)]` build without `ecs`; the App extension,
`NetSender`, `NetMessages` and `ValidateContext`'s `target`/`world` fields are `ecs` (`ValidateContext::core` builds the core
context). The dependency assertion reads `-e normal,build` (stricter than the `-e normal` above) and also forbids
`regia_platform`, `regia_audio` and `bevy_ecs`, so an indirect path to the ECS fails too. The example
`examples/net_core_headless.rs` (a `NetServer`, two `NetClient`s on `LoopbackNet`, the tick driver and a derived message) runs
as the test `tests/net_core_headless.rs` with and without `ecs`. There is no `RollbackLog` on the line yet, so the example has
none.

### 3.3 Module map (new and changed modules)

| Module | New/changed | Core or `ecs` | Section |
|---|---|---|---|
| `connection.rs` -> `connection/{mod, ack, loss, pacing}.rs` | changed (transport v2) | core | 4.1, 4.2 item 7 |
| `cc.rs` (rate controller; `congestion.rs` `CongestionNotches` kept as a deprecated helper for one line) | new | core | 4.2 |
| `stream.rs` (reliable streams, reassembly caps) | new | core | 5 |
| `quality.rs` (`PeerQuality`, grade, events) | new | core | 13 |
| `lockstep.rs` (`LockstepHost`, `LockstepSeat`, frames, delay control) | new | core | 7 |
| `desync.rs` (`DesyncHash`, probe, verdict, bundle) | new | core | 7.6 |
| `rollback.rs` (`RollbackLog`, `VisualOffset`) | new | core | 9.5 |
| `migration.rs` (election, frame cache, successor journal) | new | core (journal hooks in `ecs`) | 10.5 |
| `relay.rs` (allocations per client, permissions, protocol v2) | changed | core | 10.6 |
| `nat.rs` (candidates, checks, nomination) | new | core | 10.7 |
| `version_probe.rs` | new | core | 12.2 |
| `server.rs`, `client.rs` (path validation, feedback, slot reclaim, quality) | changed | core | 6, 10, 13 |
| `replication.rs`, `send.rs`, `relevance.rs`, `interp.rs`, `predict.rs`, `lagcomp.rs` | changed | `ecs` | 8, 9 |
| `external_replication.rs` (relevance and rate classes over `SnapshotEncoder` for a non-ECS sim) | new | core | 8.6 |

## 4. Transport v2

Wire version **5** (§ 12). Everything in § 4 and § 5 lives inside the sealed record (the Noise record layer and its cleartext
session token and counter are unchanged), so path validation and rebinding keep working.

### 4.1 Packet numbers, acknowledgements and loss detection [PUB]

Baseline [REG]: a 9-byte header (`connection.rs:44`, :465-478): 16-bit sequence, 16-bit newest received, 32-bit mask, flags.
Faults: a 64-packet poll cannot be acknowledged by one reply, so 31 delivered packets are counted lost (adversarial row 1);
loss is declared only after a fixed 2 s or 32 packets (`connection.rs:53`, :741); an ack that arrives after the loss timeout
gives a 2,000 ms RTT sample (adversarial row 3); every due fragment is resent every RTO with no backoff (:398).

Design:

1. **Packet numbers** are 62-bit, monotonically increasing per direction, never reused, sent truncated (1, 2 or 4 bytes) and
   decoded against the largest acknowledged number (RFC 9000 § 17.1, Appendix A). A retransmission is a new packet number
   carrying the old frame (RFC 9002 § 2): an ack is never ambiguous.
2. **Ack frames** carry the largest acknowledged, the ack delay (microseconds, scaled), and up to 32 ack ranges (gap, length)
   (RFC 9000 § 19.3). A receiver acknowledges every second ack-eliciting packet or within `max_ack_delay` (default 10 ms;
   immediately on reorder) (RFC 9000 § 13.2.1-13.2.2). Ack-only packets are not ack-eliciting and are not themselves acked.
   Built (ADR-10050, wire 6): the frame also carries `arrival`, a `u16` after the ack delay: the low 16 bits of the
   receiver's millisecond clock when the largest acknowledged packet arrived (0 in a frame of older ranges). Two bytes in
   every frame, not negotiated; the one-way delay measurement of RFC 6817 § 3.1.
3. **Receive timestamps** (optional, negotiated): the ack frame may carry the receive time of up to 16 packets, relative to the
   largest acknowledged, for the delay-gradient signal of § 4.2 (the per-packet feedback shape of RFC 8888 and the
   transport-wide feedback GCC uses).
4. **Loss detection** (RFC 9002 § 6.1): a packet is lost when a packet sent at least 3 later is acknowledged (packet threshold
   3), or when it was sent more than `9/8 x max(smoothed_rtt, latest_rtt)` (floor 1 ms) before an acknowledged packet (time
   threshold). Lost frames that carry reliable data are queued for retransmission at once (not at the next RTO).
5. **Probe timeout** (RFC 9002 § 6.2): `PTO = smoothed_rtt + max(4 x rttvar, 1 ms) + max_ack_delay`, doubled on each
   consecutive expiry (the backoff RFC 6298 § 5.5 asks for), reset by any ack. A PTO sends one or two probe packets with the
   oldest unacked reliable data, never the whole window. Persistent congestion (RFC 9002 § 7.6) collapses the rate controller to
   its floor (§ 4.2).
6. **RTT samples** only from the largest newly acknowledged packet when it is ack-eliciting, minus the peer's ack delay (clamped
   to `max_ack_delay`), never from a packet already declared lost or retransmitted (RFC 9002 § 5.1-5.3). `min_rtt` is kept over
   a 10 s window for § 4.2. RTTVAR starts at `rtt/2` (RFC 6298 § 2.2; kept).
7. **Spurious-loss undo**: a packet declared lost and later acknowledged undoes the loss count and, if it was the only cause of
   a rate cut in the last RTT, restores the rate (DO27 rule A7, "reorder is never loss" [D27]).

Built (N101, ADR-7950; item 5 backoff only): v0.5.x fragments double consecutive timeout waits
up to 8 s, resetting on newly acknowledged traffic. Repeated waits have a 250 ms floor; the
existing estimator and first-retry timing stay intact. Four initial recovery packets per poll
grow on acknowledged rounds, with an additional 32 reliable packets per rolling second and
empty ACK coalescing. Full v2 probes/ACKs/rate control remain N201/N202; no wire bump here.

**Built (v0.5.x, ADR-7949):** the unchanged v1 header bounds data to 32 outstanding sequence
positions and reserves its 33rd for an ack response. Only represented ack holes or time expiry
count loss; lost reliable fragments become due immediately. Only the largest new ack can sample
RTT, excluding lost/expired packets and retransmitted data. The client bounds the RTT lead to the
input horizon minus three; custom v1 horizons use `set_server_input_buffer_ticks` because the v1
handshake does not advertise the server's setting. `inputs_too_far_ahead` is exposed per endpoint
and peer. These are v1 repairs; the v2 packet/ack layout and spurious-loss undo above remain future work.

**Built (v0.5.x, ADR-7995; items 4, 5, 6 and 7 on the v1 header):** one policy replaces the
outstanding-sequence bound of ADR-7949, the 32-packet burst cap of ADR-7988 and the recovery
trigger of ADR-7950. A receiver acknowledges every range of 33 packets it owes, oldest first
and each twice, so nothing delivered is left unnamed and no sender-side horizon is needed.
Loss is declared by packet threshold 3 or 9/8 of max(smoothed, latest) round trip after a
later acknowledgment, never by silence; a late acknowledgment undoes it (item 7) and widens
both thresholds to the reordering seen. Lost fragments are resent at once, at most once per
round trip and 8 packets per poll; timer resends are probes (4 packets per poll, 32 per
second) with the backoff of item 5 and never undercut 9/8 of the measured round trip.
Reliable bytes in flight are bounded by a window of twice the bytes acknowledged within one
round trip (64 packets at first, floor 4), which random loss does not shrink (the stand-in
for § 4.2 until the rate controller lands). Outage recovery needs a measured path and either
a second consecutive timeout or losses sent that long apart; its cap scales from the earlier
delivery rate and it ends early when nothing was lost. Still future work: packet numbers, ack
frames with ranges and ack delay, receive timestamps, the rate controller and pacing by time.

**Built (N201, ADR-8184; items 1-7, wire 5):** refinements against the text above. Item 1: the
number is sent in 2 or 4 bytes with four times the distance to the largest acknowledged number
(never 1 byte): an unsealed link has no packet protection to reject a misdecoded number, and a
1-byte number arriving after 200 newer packets acknowledged the wrong packet. Item 2: received
ranges older than the 32 one frame names leave first in frames of their own, flagged so no
round-trip sample is taken from them; a lone ping (keepalive or probe) is acknowledged at once.
Item 3: asked for in band by a packet flag (`LinkConfig::receive_timestamps`), read with
`Connection::take_receive_timestamps`. Item 5: the probe timeout is also clamped to
`resend_min_ms..=resend_max_ms` and never under 9/8 of the round trip; keepalives are
ack-eliciting pings, so an outage is probed by pings and the data is repaired from the first
acknowledgment after it. Item 6: packets that carried a resent fragment are also not sampled. Added for the E2 bar
(row T5), which loss detection alone cannot meet at 5 % random loss (a repair needs a round
trip plus the one-way delay): a reliable fragment of at most 256 bytes rides once more in the
next packet that leaves 1 to 20 ms after it (no extra datagram, not a resend; E2 p95 1.1x the
one-way delay). Still future work: the rate controller and pacing of § 4.2, DPLPMTUD (§ 4.3).

Built (net-lockstep-upload-acks, ADR-8423): item 2's frames stop naming numbers the peer has confirmed hearing about
(RFC 9000 § 13.2.4): when the peer acknowledges a packet of ours whose frame named a span, the span is no longer
named, except packets that filled a gap inside it after the frame left (an arrival-indexed log of the last 64; a
confirmation whose log may be incomplete removes nothing). A frame whose largest is not the newest arrival goes with
ack delay 0 and the "older ranges" flag (no round-trip sample); with nothing left to name, a packet carries no frame.
Encoding and flags are wire 5's own. Lockstep host frames: 6.0-6.3 bytes per datagram (before: up to 32 ranges, about
70 bytes on a 3 % loss link); `LinkStats::ack_frame_bytes` counts them.

**Built (ADR-8400, repair copies):** each repair of a reliable fragment of at most 256 bytes (after a loss verdict or a probe
timeout) also rides once more in a later packet within 20 ms or one tick, whichever is longer; after a loss verdict the copy
leaves in a packet of its own at the next poll if no other packet carries it. At 1.5 s one-way and 20 % loss with the game
quiet, a lost repair otherwise costs another 3 s round trip: the soak's rtt-extreme row failed on 5 of 100 seeded links
without it and on none of 200 with it.

### 4.2 Rate controller and pacing [PUB]+[NEW]

Baseline: no congestion control on any path (E1: 2x overload held for 30 s with 28.9 % loss); `CongestionNotches`
(`congestion.rs`) is used by nothing in the engine and cuts on loss alone, so it collapses a lossy Wi-Fi link that has plenty
of bandwidth to 8 of 140 entities (F7); a listen server's peers share one uplink with no aggregate budget and collapse past 32
clients (F5). DO27's live path has none either; its notch rule (A4) only trims the dormant server's entity count.

Options considered (ADR-7905 lists the reasons in full): LEDBAT (RFC 6817) is a scavenger that yields to TCP and would make a
game lag whenever the household downloads; TFRC (RFC 5348) is loss-equation driven and cuts on random wireless loss; plain BBR
(Cardwell et al., 2016) handles random loss well but needs probing bursts and was built for bulk flows, while game traffic is
app-limited most of the time; GCC (draft-ietf-rmcat-gcc) was built for app-limited real-time media and combines a
delay-gradient detector with a loss bound that ignores loss under 2 % and only cuts above 10 %.

Decision: **a game rate controller (`cc.rs`) in the GCC family, with a BBR-style delivery-rate estimate and minimum RTT**:

1. **Signals per feedback round** (each ack frame, at most one decision per `max(srtt, 50 ms)`): delivery rate (bytes newly
   acked over the interval, RFC 9002 style sampling, as BBR's delivery-rate estimator), queueing delay `q = latest_rtt - min_rtt`,
   the one-way delay gradient from receive timestamps when negotiated (trendline filter over the last 20 packet groups, as GCC's
   arrival-time filter), and the loss fraction of the round.
2. **States**: `Increase`, `Hold`, `Decrease` (GCC's remote-rate state machine). Overuse = gradient over the adaptive threshold,
   or `q > 25 ms` and rising for two rounds. Overuse -> `Decrease`: target = `0.85 x delivery rate`. Normal -> `Increase`:
   multiplicative +8 % per second far from the last cut, additive (one packet per RTT) near it. Underuse -> `Hold`.
   Built (ADR-8859): the delivery rate a cut reads is the larger of the round's and the latest second's (GCC's incoming-rate
   window), so a round that acknowledged one packet does not set the cut. On a server that splits a shared uplink (item 8)
   the round alone still decides.
   Built (ADR-10050, wire 6): every acknowledgment frame carries the arrival of its newest packet (16 bits of the
   receiver's millisecond clock; § 4.1 item 2), and the sender reads its own packets' one-way delay from it
   (`cc::OneWayDelay`). Round trips still decide; an overuse verdict of an app-limited sender (nothing held, under half
   the target sent) stands only once the smallest of its latest six one-way samples sits 12 ms over the 10 s
   minimum, and waits until then. A sender that fills its target, and every peer of a sender that splits a shared uplink
   (item 8), is read as before: measured, the more exact readings cut later and the congested rows have no margin for
   that (the ADR's table).
3. **Loss bound** (GCC's loss-based controller): loss < 2 %: no effect; 2-10 %: hold; > 10 %: `target x (1 - 0.5 x loss)`,
   **but only when the delay signal also shows queueing or the delivery rate is under the target**. Loss with no queue growth on
   an underused path is treated as non-congestive (wireless) loss and answered with redundancy (§ 6.1), not a rate cut. This is
   the F7 fix.
4. **Floor** (RFC 8085 § 3.1 and the circuit breaker idea of RFC 8083): the essential class (inputs, tick sync and lockstep
   frames, acks, keepalives) is always allowed `floor_bytes_per_second` (default 16 KiB/s up, 32 KiB/s down, project-settable),
   whatever the controller says. If the path cannot carry even the floor for `circuit_breaker_s` (default 10 s) with loss over
   50 %, the session moves to `Interrupted` (§ 10.2) instead of flooding.
5. **Recovery never slower than the cut** [D27 A4]: after a loss-driven cut, the rate returns to the pre-cut value within at most
   the time the cut took plus 2 s once the loss clears (DO27's test: floor within 3 s at 10 % loss, back within 5 s of clearing).
6. **Start**: initial rate `min(project rate, 256 KiB/s)` with a 2-RTT startup that doubles while the delivery rate keeps up
   (BBR's startup, bounded), so a join does not blast the path (E2b).
7. **Pacing**: every datagram leaves through a token bucket at `1.25 x target` with a burst of at most 2 packets after an idle
   period (RFC 9002 § 7.7). `Connection::poll` returns only what the pacer allows; `next_send_at_ms()` tells the endpoint when to
   poll next. All budgets are per sim tick or per pacer time, never per render frame [D27 A1].
   ADR-8692: the burst floor is 4 packets (it was 2), so a 4 KiB command an idle client queues leaves in one poll instead
   of trickling out at an app-limited target (RFC 9002 § 7.7 allows up to the initial window).
   Built (ADR-8859): a foreground reliable message the pacer lets start finishes in the same poll, up to 4 packets from
   its start, when the buckets are spent; the burst is charged to the buckets and to an excess allowance that only unused
   target fills (RFC 2697's excess bucket; none under a shared uplink's share). The bucket's level alone was not enough:
   a client's re-bundled inputs drain it.
8. **Aggregate budget** (F5): `NetServer` owns a `SharedUplink` controller fed by the sum of the peers' delivery rates and the
   host's own uplink signal (queueing delay on all peers rising together); each peer's allowance is `min(peer target, fair share)`
   with max-min fairness and a priority weight per peer (spectators lowest). Listen servers enable it by default; dedicated
   servers may set `server_rate_bytes` (already in `SendConfig`).
   Built (ADR-10097): the aggregate, the shares and `uplink_bytes_per_second` are rates on the wire (each peer's target
   and delivery rate times `Connection::wire_bytes_per_body_byte`, the share divided by it; a peer's own controller keeps
   steering bodies, by measurement). With 16 or more delivering peers (`cc::UPLINK_CROWD_PEERS`) the aggregate reads the
   delivered rate over 250 ms, keeps its estimate of the uplink through a hold, asks again after 15 s with a 150 ms
   probe at 12.5 % over the target (a 5 ms rise of the delivering peers' median queueing delay confirms the estimate;
   no rise with 2 % more delivered raises it), and takes its first target from the median peer's loss or from four
   peers that have sent for a second without an acknowledgment while they still talk (`Connection::unanswered_ms`:
   the tail of each flush into a full drop-tail queue). Under 16 peers it is as ADR-8490 and ADR-8733 left it.
9. **What consumes the rate**: the scheduler (§ 5.3) hands out `target - essential` to replication (`tick_budget = min(project
   rate, controller allowance)`, replacing the static `peer_bytes_per_second` at `settings.rs:129`), streams, voice and bulk, in
   that priority. The game sees the allowance in `PeerQuality::send_allowance_bps` (§ 13) and can lower its own rates.

```rust
/// Per-peer send-rate controller (spec 120 § 4.2). Pure: fed acks and time, asked for an allowance.
pub struct RateController { /* private */ }
impl RateController {
    /// Feeds one ack round: newly acked bytes, the latest RTT sample, the minimum RTT and the round's loss.
    pub fn on_round(&mut self, now_ms: u64, sample: &RoundSample);
    /// Bytes per second the peer may send now, the essential floor included.
    pub fn target_bytes_per_second(&self) -> u32;
    /// Which state the controller is in, for quality signals and tests.
    pub fn state(&self) -> RateState;
}
```

**Built (N202, ADR-8301; items 1-9, no wire change):** `regia_net::cc` (`RateController`, `Pacer`,
`SharedUplink`, `TrendlineFilter`, `RateSettings`), on by default. Refinements: the queueing delay is the round's
smallest round trip over the 10 s minimum (jitter delays some packets, a queue all); besides the 25 ms rule and the
trendline, a round-trip delay gradient over 2 % across four rounds (read on a two-round floor, because an endpoint that
reads its socket once per tick adds up to a tick to every sample) counts as overuse for a sender that pushes the path;
a queue standing over 25 ms holds; the loss cut fires at 10 % or more, and without a queue a saturating sender never
targets above what the round delivered (a policer), unless a shared uplink caps it; an app-limited target stays within
1.5x the mean delivery rate plus 10 kbit/s (GCC's acknowledged-rate bound); the pacer at 1.25x is one tick deep and a
second bucket at 1.0x (two ticks) holds a saturating sender's average to the target; the first burst is the initial
window. The floor is the essential class's own bucket; the target's own minimum is 4 KiB/s. Item 9 is
`replication`'s `min(project rate, allowance)` (the static rate is `client_rate_bytes`, not
`messages.peer_bytes_per_second`, which bounds inbound bytes). The allowance and queueing delay are `LinkStats` fields
until `PeerQuality` (§ 13.1) lands. Settings: `config/net.ron` v3 `session.rate_control` (folded into the unreleased v3, ADR-7724). Further refinements
(ADR-8301 extension): "the delivery rate is under the target" means the delivery rate stopped following the send rate,
so a loss cut without a standing queue is a probe, given back after about six rounds when the loss stays (random loss)
and kept when it falls (a policer); with a standing queue a saturating sender is also clamped to what the round
delivered; the app-limited bound skips an idle link (delivery under the floor), app-limited rounds neither double nor
count in the startup, and its full-pipe exit drains to the best delivery rate; a round after an outage of 1 s or more
restarts the signals and the startup without a cut; the queue thresholds rise with the path's delay noise above 10 ms;
the shared uplink reads its median on a two-update floor, holds at the delivered rate while a queue builds, and grows
only while its peers deliver 95 % of it. The controller counts body bytes, as the allowance the game sizes its payload
from does (counting headers in both was measured and double-counts them).
ADR-8658: while the smoothed loss is 2 % or more, the round-trip delay gradient reads each point on the floor reaching
back to 64 acknowledged packets, as the threshold detector does (ADR-8490); thin rounds on a lossy, jittery path climbed
by jitter alone and read as a growing queue.
ADR-10157 (v0.7, v07-net-staleness-rtt-random-loss): item 3's cut without a queue needs a sender that filled its target in
this round and the one before (a feedback round can hold one tick of a game's sends more than its length, and a join's
first round acknowledges packets of no round), 10 % loss since the latest such cut took effect (one loss event, one cut)
and reads the delivery rate over at least 32 resolved packets. The probe compares the loss after the cut with the path's
long-run loss (the trigger's own reading only when it is more than that loss explains, 3 standard deviations) by a
sequential probability ratio test (5 % error either way; it runs until a thin sender's packets can tell). A "random"
verdict excuses loss up to the long-run level since the cut for the 10 s hold and keeps the episode's earlier cut owed
(item 5). The startup's filled rounds follow the same two-round rule unless the pacer held data.
The host's own signal needs several peers (ADR-8733 (2)): the shared uplink reads the median queueing delay of its peers
only while at least four peers that delivered data in their latest round see a queue over 25 ms in the same round (`cc::host_queue_ms`, `UPLINK_SHARED_QUEUE_PEERS`) and those are a strict majority of the peers, counting one witness per address block (`cc::host_queue_ms_by_block`, ADR-8883: half the peers, or four connections of one machine, are not the host's uplink, § 1.1). A queue at the host's uplink delays every peer behind it at
once; one that a few peers see is on their own downlinks, and their own controllers answer it. The median over every
peer cut a listen server's aggregate at a client's join, when one client behind a slow downlink had delivered anything,
and wedged all of them (congested/8/listen).
**v0.7 integration refinement (ADR-10327, proposed):** the strict-majority condition applies when the median
exceeds 25 ms. With at least four delivering witnesses over 25 ms, a median at or below 25 ms still feeds
ADR-10097's cautious hold as it rises. Address-block folding happens first; four connections of one machine
still supply only one witness. Both the half-peers slow-downlink regression and the small-median hold tests
remain acceptance checks.
**Built (v07-net-scale-128, ADR-10081 (5)).** The gate decides whether a host queue may cut or hold the aggregate; whether
the queue floor *rose* is read on the medians before the gate. Per-peer readings sit about one tick apart, at the
threshold, so the witness count crosses the gate from round to round; read on the gated floor, every return from a
silenced round was a rise and held the aggregate at 97 % of delivered again, down to the floor within 20 s on a
16-player listen server (`listen-uplink/16`: 0 of 10 seeds passed with the gate alone, 10 of 10 now).
A round without a round-trip sample of its own is no standing-queue evidence (ADR-8400): at heavy loss most acknowledged
packets carried resent data (not sampled), and re-reading the latest rounds' floor made every such round a congestion
cut.
Queue readings span enough samples (ADR-8490): the standing queue that turns a loss cut into a congestion cut is read
on the floor of the latest rounds back to 32 acknowledged packets (at most eight rounds), and while the path loses
packets (smoothed loss 2 % or more) the threshold detector reads back to 64 packets and takes the round's own reading
over that floor as its delay noise. The smallest of a few round trips on a jittery path sits over the minimum by the
jitter of both directions; read as a queue it held a saturating sender on a lossy, jittery, unlimited link at the
minimum rate. A lossless path keeps the round's own reading.
Headroom under the bottleneck (ADR-8490): a cut with queue evidence (a delay-driven decrease, or a loss cut with a
standing queue) measures the bottleneck as the delivery rate of the latest second; for 15 s after the latest such cut a
saturating sender's growth stops at 97 % of it (a loss cut's recovery to its pre-cut rate, item 5, is still owed in
full). Growth to the full rate found that rate by
overshooting it, and the second that held the overshoot offered more than the link carried. The estimate lapses after
15 s (growth tests the link again), after an outage restart and when a probe shows the loss was random.
The essential class never starves the rest (ADR-8534): the target never falls under the essential class's own
rate (up to the floor) plus 4 KiB/s. The essential class leaves inside its floor whatever the target says, so a
target under it lowered nothing that was sent and left the reliable stream (repairs, lockstep catch-ups) no
allowance at all; one noisy overuse cut on a 6 Mbit/s lossy link took a lockstep host's target to 4.6 kB/s under
12-15 kB/s of frames. A congestion cut's recovery (item 5) owes its pre-cut rate only up to 97 % of the rate the path
delivered at the episode's first cut with a standing queue; four rounds sent there with no queue show the loss was
transient and the rest is owed at once (DO27 A4), a queue there ends the recovery at the headroom. Under a shared
uplink's fair share (item 8) a connection's pacer keeps no first burst above its depth. Lockstep: a frames datagram
that continues frames the same pass put on the stream goes in the ordinary class, behind the stream (the essential
class leaves first, and a seat that sees frames past its chain reports a hole).
Startup and the app-limited bound (ADR-8592, item 6): the bound (1.5x the latest second's delivery plus 10 kbit/s)
lowers an unconfirmed start rate to what the game uses but no longer ends the startup. The startup grows, and counts
toward its full-pipe exit, only on rounds the sender filled: the pacer held data back (the essential class counts:
inputs that outrun the target are not app-limited) or it sent at least 80 % of the target. A round with a fifth of
the target unused and nothing held says nothing about the pipe (BBR skips app-limited samples), so a flat game is not
drained to its own demand, and a client whose first data round was half empty is not left at 1.5x that round, under
its demand, while the 2-10 % loss band holds it there.

**Built (v07-net-staleness-queueing, ADR-10145; items 2, 6 and 7, no wire change).** A deep standing queue is drained:
when a delay-driven cut finds 100 ms of queue or more, or such a queue stands for two rounds while the sender fills its
target, the controller answers with `1 - queue / 400 ms` of the bottleneck's delivery rate (at least 0.25, at most
item 2's 0.85) for the time the queue needs at that rate plus a round (at most 1.5 s), reads the queue on the round's
smallest round trip and on wire 6's one-way delay when that is smaller, ends the drain on the first round whose packets
met under 12 ms of queue, and then returns to the target item 2 left (the target itself is never lowered by a drain). A
queue the drain does not move (80 % of its depth left at the deadline) is not the sender's: no drain for 15 s. Item 2's
cut is unchanged under 100 ms of queue. Item 6: a startup also ends on the first round whose smallest sample shows
100 ms of queue (RFC 9406's delay-increase exit), not only on two rising rounds, and its drain starts at the lowest
gain; a second deep queue within 15 s of a drain that emptied the first confirms the bottleneck, and a loss cut's
recovery (item 5) then ends at the headroom. Item 7: the first burst is the initial window, and under a lowered
`start_bytes_per_second` at most 300 ms of it (four packets at least); the default 256 KiB/s sets no bound. Measured on the soak's congested row (a 16 KiB world blob per join into 256 kbit/s behind a 16 KiB
buffer): a join was about 2.4 s over the staleness limit and is 0.8 s; `staleness_p99` 22 ticks, now 13 to 14 on four
seeds, and 7 (the bar passes) with the start rate at 32 KiB/s. The default start rate is unchanged: ADR-10145 asks
for the ruling, with the price of each choice.

**Built (v07-net-staleness-queueing, ADR-10156; items 6 and 7, off by default, no wire change).** The first flight:
with `RateConfig::first_flight` on, a sender offers a path that has not answered yet the essential floor's rate and
a first burst of 300 ms of it (eight packets toward a client). A first feedback round the sender filled in which the
path acknowledged under three quarters of what was due (the path fell behind the flight) starts the startup at that
round's delivery rate (at least the floor, at most `start_bytes_per_second`); a path that kept up, and an unfilled
first round, leave the start rate. On the soak's congested row at the default 256 KiB/s start: `staleness_p99` 7, 7,
8, 8 ticks on four seeds (the bar passes; 13 to 14 with it off), the join's world blob 0.10 to 0.11 s later on a
good link (0.19 to 0.21 s with the 32 KiB/s start). It ships off because base tests pin the blind
initial window of a default connection; ADR-10156 asks for the default as option (d) of ADR-10145's ruling.

### 4.3 Packet size and path MTU [PUB]

Kept: 1,200 bytes including the sealed header is the floor on every path (RFC 9000 § 14; IPv6 minimum 1,280). New:
**DPLPMTUD** (RFC 8899) as an opt-in per server (`mtu_probing = true`): padded probe packets of 1,280, 1,400 and 1,452 bytes,
confirmed only by an ack, never carrying data that matters, black-hole detection drops back to 1,200 after 3 consecutive probe
losses or any loss burst on full-size packets. The QUIC option starts at 1,200 and enables its own discovery (adversarial row 5).
A socket send that fails or would block is counted as `socket_drops` on the peer, not as sent (today `socket.rs:84`,
`server.rs:627-634`), and the packet number is not consumed for a packet that never left.

**Built (net-v2-pmtud, ADR-8300).** `regia_net::pmtud` plus the probe paths in `Connection`; opt-in through
`LinkConfig::mtu_probing` / `LinkOptions::mtu_probing` / server.toml `[net] mtu_probing` (optional key of the unreleased v4, ADR-7724). Refinements: probes
wait for the first round-trip sample and are a ping plus a padding record (tag `0x00`, wire 5 reused); a probe counts as
lost only when a later packet was acknowledged, otherwise (an outage) it is inconclusive and resent, so an outage never
reads as a black hole; "any loss burst on full-size packets" became two rules: the first lost packet above the base puts
the size in doubt (data at 1,200 from the next packet, a confirmation probe at once; three lost confirmations drop to the
base) and three large losses in a row around an acknowledged small packet drop at once; reliable fragments stay cut for
1,200 so a fall back strands nothing; confirmation every 5 s without a large acknowledgment, raise retried after 600 s,
search again 60 s after a black hole. The path MTU is in `LinkStats::path_mtu` (the confirmed size since ADR-10097: a doubt after a lost full-size packet no longer reads as the base size; `PeerQuality` of § 13.1 is not built
yet). Real UDP sockets do not set don't-fragment yet (follow-up). Tests: `crates/regia_net/tests/pmtud.rs`.

**Built (v07-net-portmap-df-per-os, ADR-8893; closes KI-NT-028).** Every `UdpTransport` sets don't-fragment (RFC 9000 § 14,
RFC 8899 § 3): `IP_MTU_DISCOVER` = `IP_PMTUDISC_PROBE` on Linux and Android (the kernel's cached path MTU is ignored, so the
endpoint's own probing decides), `IP_DONTFRAGMENT` on Windows, `IP_DONTFRAG` on macOS, and the IPv6 options on IPv6 sockets;
`SocketBufferReport::dont_fragment` and `net.socket_buffers` report the read-back. A send the OS refuses as too large
(`EMSGSIZE` / `WSAEMSGSIZE`) lowers `max_datagram_size` toward that destination below the refused size, so the endpoint
reads it as a size refusal (`TxRefused::TooLarge`: a refused probe ends the search at the size below) and never as a socket
drop that would resend the same probe. QUIC: the UDP socket under QUIC sets don't-fragment too; QUIC runs its own discovery
inside the connection, so `mtu_probing` reaches it as a switch (`QuicListener::set_mtu_probing`,
`QuicDatagramSocket::connect_with_mtu_probing`; off pins 1,200) and the plain constructors keep discovery on as above.
Tests: `regia_net/tests/dont_fragment.rs` (an unprivileged user+net namespace with a 1,400-byte loopback: a plain socket's
1,400-byte datagram arrives fragmented, the transport's is refused, and probing endpoints settle at 1,280 instead of 1,452).

**Built (N204, ADR-8407; socket drops only, no MTU probing).** `UdpTransport::send_to` returns `WouldBlock` (and a
second stale `ConnectionReset`) as errors; the endpoint counts every refused datagram in `EndpointTotals::socket_drops`
(not in `datagrams_out`). With the conditioner off, a refused connection packet is `TxRefused::SocketDrop`: the poll
ends, nothing of the packet is committed, its packet number is not spent and `LinkStats::socket_drops` counts it; a
conditioned (delayed or duplicated) datagram refused later is counted at the endpoint only, like a network loss.

### 4.4 Sockets [D27]

`UdpTransport` sets the receive buffer to 2 MiB and the send buffer to 512 KiB, reads both back, and warns once if either is
under half (DO27 rule A8); DSCP Expedited Forwarding (`0xB8`) on a best-effort basis where the platform allows. Windows and
Android code stays behind `platform/<os>.rs`.

**Built (N204, ADR-8407).** `socket::SocketReport::configure` sets and reads back both buffers and the DSCP mark
through `socket/platform/{unix,windows,other}.rs` (hand-declared `setsockopt`/`getsockopt`; no new dependency); the
report is `NetStatus::socket`, and `undersized` (either read-back under half) logs one warning per process. Linux
reports twice the stored size, so a stock `rmem_max` of 208 KiB reads back 416 KiB, which is under half of 2 MiB.

### 4.5 Invariants as tests [D27]

DO27's anti-pattern table becomes tests in `regia_net` (each a named test, § 15.4): A1 all net I/O and budgets are per sim tick
(the same client at 30 fps and 300 fps sends the same bytes and datagrams per second within 2 %); A2 reliable messages per
second capped per class, the counter names the class; A3 an always-changing entity costs no more than a resting one (no
reliable dirty-object path); A4 bounded back-off, recovery never slower than the cut; A5 never removed for ping or loss; A6 no
duplicated datagrams on the wire; A7 reorder is never loss; A8 socket buffers read back; A11 mixed-version peers refused with a
readable reason.

**Built (N204, ADR-8407): A1 and A2.** A1 already held at the N204 base: `frame_rate_io` runs the same `NetLink`
client at 30 and 300 fps (30 Hz simulation, 30 replicated entities changing every tick) and the datagrams and bytes per
second agree within 2 %, because the bridge sends per tick and acknowledgments, keepalives and pacing are timed, not
per poll; the test guards it, and the endpoint still polls every frame (gating the flush per tick would delay
acknowledgments by up to a tick at high frame rates for no measured gain). A2: `StreamSpec::msgs_per_s` caps the messages
a stream starts in any sliding second (240 by default, chat 30, bulk 0 = none); held messages stay queued, and the
stream's `StreamStatus::{class, msgs_capped, msgs_last_second}` name the class.

## 5. Channels, streams and fragmentation

### 5.1 Classes [PUB]

Baseline: two physical channels (`connection.rs:36-41`); `ReliableUnordered` rides the one ordered stream (`message.rs:55-58`);
E2b measured a 256 KiB transfer holding small RPCs for up to 3.4 s on a capped link. Spec 70 § 1 claims four channels and
congestion control: § 18 corrects it.

Transport v2 has:

| Class | Delivery | Used by |
|---|---|---|
| `Unreliable` | at most once, any order, never resent, never fragmented (refused over `max_unreliable_bytes()` at the API) | input bundles, state frames, voice, tick sync |
| `UnreliableSequenced` | at most once, older-than-newest dropped on receipt | snapshots and lockstep frame bundles where only the newest matters |
| Reliable stream `s` (ordered or unordered per stream) | exactly once; ordered within the stream only | see below |

Fixed streams (ids 0-7 reserved by the engine; games declare up to 24 more, 32 streams per connection in all):

| Stream | Order | Priority | Carries |
|---|---|---|---|
| 0 control | ordered | 0 (highest) | session control, kicks, travel, path validation answers |
| 1 inputs | unordered | 1 | the reliable input lane of ADR-7896 (lane 3), large commands |
| 2 game RPC | ordered | 2 | `NetMessage` reliable messages (spec 70 § 6) |
| 3 replication control | ordered | 2 | spawns, despawns, oversize records, ownership, join snapshots (ADR-8624) |
| 4 lockstep | ordered | 1 | lockstep membership, delay changes, desync probes (frames themselves go unreliable, § 7.2) |
| 5 chat | ordered | 3 | chat, spectator text |
| 6 bulk | ordered | 4 (lowest) | streams (spec 77 § 8), save transfer, large transfers no join waits on |
| 7 unordered RPC | unordered | 2 | `ReliableUnordered` messages, which now really are unordered |

A loss on one stream never delays another (RFC 9000 § 2.2). Retransmissions of a higher-priority stream go first.

### 5.2 Reassembly caps and deadlines [PUB]

Baseline: any fragment id within 1,024 of the next expected is stored with no byte cap and no deadline (E6: 2.13 MB held by one
connection after 70 s; adversarial row 16: a message whose last fragment never comes wedges the reliable stream forever).

Per stream: a byte cap on buffered out-of-order data (`stream_window_bytes`, default 256 KiB, bulk 1 MiB) enforced on **every**
fragment stored, not only on in-order assembly; per connection: `connection_buffer_bytes` (default 2 MiB); per server:
`server_buffer_bytes` (default 64 MiB, or 32 x max_peers KiB, whichever is less). A fragment over any cap is a protocol error for
that peer (dropped and named, § 11). A stream whose next expected fragment has not arrived for `max(4 x PTO, 2 s)` of the
peer's traffic while later ones are buffered is a stall: the connection fails with `reassembly_stalled` on the peer's stats.
An honest sender repairs a lost fragment about every two PTOs (loss detection, § 4.1), and on a lossy path its repairs
can be lost too, so the deadline is stretched by the peer's measured packet loss `p` (missing packet numbers among its
newest 256, the first one free): times `n / 2`, `n` the smallest number of attempts with `p^n <= 1e-5`, clamped to
`2..=8` (at most 4x). A peer whose packets all arrive gets the unstretched deadline (ADR-8521). Flow control is advertised per stream like RFC 9000 § 4 (MAX_STREAM_DATA), so an honest sender never
trips the cap.

**Built (N104, ADR-7981, v0.5.x on the one wire-3 stream).** Per connection: a fragment longer than `max_packet_bytes` less the
14 header bytes fails the link (`fragment_too_long`); stored out-of-order bytes are capped at `min(connection_buffer_bytes,
1024 x max fragment)` and checked before any record of a packet is applied (`reassembly_over_cap`); the stall clock runs on the
peer's own traffic, at most one RTO per inter-packet gap (so an outage does not count), until the next expected fragment arrives
or nothing waits, and fails the link at `max(4 x RTO, 2 s)` (`reassembly_stalled`; RTO stands in for PTO until N201). Per
server: `ServerConfig::server_buffer_bytes` (`0` = `min(64 MiB, 32 KiB x max_peers)`). Difference from the text above: a packet
that would pass the **server** budget is refused whole as if lost (nothing acked, counted in `reassembly_refused`) instead of
failing its sender, because the budget is shared and an honest peer must not pay for another peer's bytes; the per-connection
cap and the stall deadline still drop the hostile peers, and a packet that fills a gap is never refused. Failures are
`ServerEvent::Rejected` kinds (`reliable_too_long`, `inbox_full`, `fragment_too_long`, `reassembly_over_cap`,
`reassembly_stalled`), all fatal, and `LinkStats::failure`. No wire change. Per-stream caps wait for N203.

**Built (ADR-8521, the wire 5 line).** The rule above as written: the gap since the previous packet is capped at the RTO
as it stood before the packet's acknowledgments (the first sample after an outage carries the outage), and the deadline
is stretched by the measured inbound loss (`Connection::reassembly_stall_factor`, 1..=4). `LinkStats` reports
`reassembly_stall_ms` and `reassembly_stall_deadline_ms`. In the honest envelope of the seeded link test (delay < 300 ms,
jitter < 80 ms, loss < 30 %, duplicates < 5 %, reordering < 30 %, one outage up to 6 s, messages up to 20 KB) gaps peak
at 0.75 of the deadline (3.38x under ADR-7981 alone). Fuzz target `net.link_pair_impaired` checks it with two real
connections through the conditioner. Since integ-v06s-net (ADR-8572) the stall clock runs per stream (ADR-8302 below) with this deadline: the
streams lane's own 4..16 PTO loss multiple was dropped for it, and the ADR-8521 interval cap replaces its smoothed-PTO step.
Before the receiver's first round-trip sample (ADR-8658), the RTO the deadline is built from is never below the wait of its
oldest unanswered ack-eliciting packet, up to `LinkConfig::resend_max_ms`: the round trip is at least that long, so a
bulk message whose rest waits for acknowledgments that cross a deep queue does not fail the link at the 2 s floor. A
receiver that has sent nothing keeps the floor.

**Built (ADR-8764).** An authenticated packet the connection cannot parse (empty, unknown flags, a bad or cut packet number,
a bad ack frame or records) is still the peer's traffic: its gap counts toward the stall clock like a duplicate's (at most
one RTO) and nothing else of it is applied, and the measured-loss stretch is withheld until no stream stalls, so a peer
that drips such packets behind a gap fails with `reassembly_stalled` at the base deadline instead of holding its buffered
bytes while § 10.3's liveness touch keeps it connected. A packet from
an address nothing validated (ADR-8606) still counts nothing. An authenticated record that is not a payload (a `Join`
resend, or an `Accept` or `Deny` only a server sends) is the same: it runs the stall clock the same way, so a drip of
them behind a gap fails at the base deadline too (ADR-8883).

**Built (N203, ADR-8302; § 5.1, § 5.2, § 5.4 on wire 5).** The stream table is `connection::ENGINE_STREAMS` (ids 0-7 as in
§ 5.1) plus `LinkConfig::streams` (ids 8-31); `Channel::Stream(StreamId)` sends on one, `Channel::Reliable` is stream 2 (and
stream-2 messages are reported as `Reliable`). Each stream has its own 16-bit fragment ids, reassembly, order (an unordered
stream delivers a message once all its fragments are in, past any gap; a first-fragment flag marks message starts) and stall
clock (`max(4 x PTO, 2 s)` of the peer's traffic, per stream: progress on one stream no longer hides a gap on another). Wire: a
reliable record is `0x40 | stream` with first and more flags (stream 2 keeps the `0x02`/`0x82` form) and `0x04 stream
max_data` is the credit record (RFC 9000 § 19.10 shape); this rides the open wire-5 bump of N201, no new version. Flow
control: the receiver grants `consumed + window_bytes` and sends credit when that moved a quarter window past the last grant,
resending lost credit (RFC 9000 § 13.3); the sender never sends a fragment ending past the credit, and never lets the bytes its
peer may have to store ahead of gaps, summed over streams, pass `reassembly_cap_bytes`. A stream storing more than its window
is `stream_over_cap`, an undeclared stream `unknown_stream` (both fatal `RejectKind`s). Send order: stream priority, then
repairs before new data **within a priority** (the text above puts every repair first; a bulk repair ahead of a game RPC would
reintroduce the delay this section removes). Differences: (1) streams at priority 4 or lower (bulk) only fill the path's
bandwidth-delay product (windowed-max delivery rate x minimum round trip), growing while no queue shows and draining under it
when one does, as the stand-in for § 4.2/§ 5.3 pacing: E2b then gives small RPC p95 36-40 ms against a 32 ms control (1.25x;
the audit's base was 96x), but the 256 KiB transfer runs at about a third of the 128 KB/s link (5.7-8.2 s); (2) the bulk
stream's largest message is the link's `max_reliable_message_bytes` (256 KiB by default), not 16 MiB: session streams chunk
their own data and the per-stream admission cap of `max_reliable_in_flight` fragments would refuse a larger message anyway;
(3) mapping: `NetSession` puts session streams on bulk and every other session message on control at its transport (the
session core stays transport-agnostic), the replication bridge's reliable frames ride stream 3, the reliable input lane stream
1; the ECS runtime still batches `ReliableUnordered` messages into the bridge's frame on stream 3, so at that layer they are
not yet unordered (open item).

**Built (wire 6, ADR-10052 (3): the open item above).** The ECS runtime sends `ReliableUnordered` messages in their own
`RUMSG` frame (tag 9: tick, message list without sequence numbers), both directions, on the reliable-unordered class,
which the bridge puts on stream 7 (`bridge::BRIDGE_UNORDERED`) as one part: an unordered stream does not keep a frame's
parts in order, so a part of several there is refused (`FramePartError::SplitUnordered`, the peer dropped like an
oversized frame). Frames are cut at `runtime::MAX_RUMSG_PAYLOAD` (one part less 512 bytes of framing, at most 32
messages); one message past it rides its tick's ordered frame (exactly once, not unordered). A received unordered
message waits for its target entity (`unresolved_wait_ticks`, as before) but never for its tick, so a stalled
replication stream does not hold it. In the seeded hub test (`tests/unordered_rpc_frame.rs`) a 30-step stall of the
ordered class delays an unordered message by at most 3 steps (it was the whole stall). No new stream: stream 7 is in
the wire-5 table already; the frame tag rides wire 6.

### 5.3 Scheduler [NEW]

Each `poll` fills packets in strict priority: acks, essential unreliable (inputs, tick sync, lockstep frames), control
stream, retransmissions by stream priority, new reliable data by stream priority with weighted round robin inside a priority
level, other unreliable (state frames, voice), bulk. Unreliable messages that do not fit stay queued, up to a byte cap and a
per-message deadline (default one tick), and are dropped only when stale (adversarial row 2: today they are discarded and the
tick-sync timestamp is set before the sync is packed). At least one packet per poll (or, inside a packet, the space of the
essential messages) is always reserved for essential unreliable data, so a reliable backlog never silences inputs or tick
sync.

**Built (v0.5.x, ADR-7949):** separate 64 KiB ordinary and essential queues charge framing as
well as payload; unpacked records expire one tick after the first poll that offered them. Inputs,
tick sync and state acks are essential; a state ack also bypasses the bridge's reliable backlog.
Tick sync stamps only after the connection reports its record packed. The v1 ack horizon can
still defer every data class while awaiting feedback; no send exceeds that representation.

**Built (N204, ADR-8407).** `Connection::send_order` sorts a poll's reliable candidates into four classes: the control
stream (0), repairs of the other foreground streams by priority, their new data by priority with weighted round robin
by fragment within a priority (`StreamSpec::weight`, default 1), and background streams (priority >= 4) last; the
poll walks the first three, then other unreliable data and the redundant copies, then the background class.
Essential data is still planned first in every packet with its own pacer floor (ADR-8301). Difference from the text:
repairs of every foreground stream go before new data of every foreground stream, as written, but bulk repairs stay
in the bulk class (after other unreliable), which keeps ADR-8302's reason for putting repairs first only within a
priority; and background data that has waited `BACKGROUND_STARVE_MS` (250 ms) without progress goes ahead of other
unreliable data in the next poll's first packet, because join snapshots ride the bulk stream and a link whose state
frames take the whole allowance must still finish a join (pure strict priority raised `congested` missed input by
+0.2 points with a transfer). Unreliable queues and deadlines are unchanged from ADR-7949.
Join snapshots moved to the replication control stream (ADR-8624): on bulk, an honest sender's held-back join
snapshot tripped the receiver's reassembly stall deadline (`reassembly_stalled`, then a rejoin) on bloated links.
Background data never borrows from the pacing buckets (ADR-8658): a background packet leaves only when the pacer and
the 1.0x budget both hold its whole size, so a bulk packet never leaves them in debt for the next poll's foreground data
(a command's reply waited one refill, 32 ms p95, on a 256 kbit/s link). The anti-starvation packet still borrows.

**Built (ADR-10125): a loss repair leads its poll; repairs are spread over a long round trip.** Difference from the
text: when the poll's first foreground candidate is a loss repair (or a probe) that the pacer, the repair budget and
the window let leave, and the credit and essential records planned in front of it would push it into the second packet
(a full fragment fills a packet), the first packet carries the repair and those records ride the second packet of the
same poll. The reservation for essential data holds: it is the poll's second packet instead of its first, and a link
with `max_packets_per_poll` 1 keeps the text's order. Reason: the datagrams of one poll reach a bottleneck back to
back, and a byte-limited queue that is full admits the first while it has room and drops the ones behind it; measured
on `asym-bufferbloat`, a full-size repair was admitted about 50 % of the time as the first packet of its tick, 5 % as
the second and never as the third, and a third of all repairs left second. Repairs per poll:
`MAX_RESEND_PACKETS_PER_POLL` (8) over the ticks in a quarter of the round trip, at least one
(`Connection::repair_packets_per_poll`): all 8 under 8 ticks of round trip, one per poll from 20 ticks on; polls less
than half a tick apart share one budget. While repairs leave one per poll, "other unreliable" data waits for them, at
most a quarter of the round trip at a time, and the poll before a repair is quiet; essential data never waits. Not
built: first transmissions are not spread (the first flight of a join snapshot still leaves as one burst, and behind
that queue it is lost whole), bulk repairs stay in the bulk class, and a repair cannot be cut smaller than the
fragment it repairs (the measured admission of a half-size packet at that queue is about 95 %; ADR-10189, the next
note, cuts the fragment smaller instead).

**Built (ADR-10189): half fragments on an unmeasured or long path.** The last "not built" above needs no wire shape:
the fragment size is the sender's choice when it cuts a message, and the receiver takes any length. A reliable message
of more than one fragment, queued while the path has no round-trip sample or while its round trip spreads repairs (8
ticks or more), is cut into half fragments (`(fragment room - 5) / 2` bytes: two with their record headers fill one
full fragment's room) while its stream then holds at most a quarter of `max_reliable_in_flight` in queued fragments;
otherwise into full fragments. A half fragment takes a full fragment's place in the send order of this section: it
shares a packet with other half fragments only, its repair leads its poll under ADR-10125's rules, and in a poll whose
repair budget is one packet the leading repair is one half fragment and nothing else, and "other unreliable" data
does not wait for it while the pacing allowance carries the repair and the packet behind it (the wait of ADR-10125 is
for full fragments, and for half fragments on an allowance that does not). `Connection::send`'s admission
(`TooLarge`, `Backpressure`) counts full fragments as before. Measured (400 `asym-bufferbloat` rows, 9,000 joins):
every join snapshot arrives, in 3.0 s at the median, 5.0 s at the 99th percentile and 7.3 s at the slowest, against
3.9 / 9.7 / 15.7 s with ADR-10125 alone and a stall deadline of 18.6 s.
"Its round trip spreads repairs" reads the path's own length (integ-v07-net, ADR-10231): the shortest round trip of the
last 10 s, never over the round trip now. A queue the sender built itself on a short path makes its round trip long too,
and that queue is the rate controller's to drain: an upload over a 256 kbit/s uplink that cut its messages small behind
its own queue stood a second queue of 1.3 s ten seconds in. A queue of other traffic that stands from the connect is in
the shortest round trip, and the spread of repairs still follows the round trip now.
Not built: a message of one fragment between half a packet and a full one is still repaired whole; first transmissions
are not spread; bulk repairs stay in the bulk class.

### 5.4 Messages larger than a packet

- Reliable: fragmented by the stream, up to `max_reliable_message_bytes` (default 256 KiB; bulk 16 MiB).
- Unreliable: **refused at the API** with `SendError::TooLarge { len, max }`; `Connection::max_unreliable_bytes()` reports the
  limit. Rejected alternative: fragmenting unreliable messages multiplies loss (at 10 % loss a 4-fragment message arrives 66 % of
  the time; ADR-7896). Producers that may exceed one packet already split at their own level (state frames, input bundles,
  `SnapshotEncoder` chunks) and fall back to the right reliable stream when a single record does not fit; the state channel's
  `payload_bytes` is clamped to `max_unreliable_bytes` (adversarial row 17) and never promotes a snapshot onto an ordered stream.

**Built (v0.5.x, ADR-7949):** state assembly clamps to each bridge peer's actual unreliable
message capacity minus game/part framing (default sealed frame limit 1150 bytes). Configured
state MTUs below 208 fail instead of silently taking the 128-byte body floor. The bridge omits
oversized unreliable frames, never promoting a snapshot; the existing explicit single-oversized-record
reliable path is retained as specified above. No settings shape or wire-version change.

## 6. Inputs and commands

### 6.1 Per-tick input (action games) [REG]+[PUB]

Kept from ADR-7896: size-aware bundles, history committed only on a queued send, the reliable input lane for an input larger than
a bundle. New in v2:

1. **Unacked-only redundancy on the NetClient path too**: the server acknowledges the newest contiguous input tick per peer in
   its feedback (§ 6.4); the client resends only unacknowledged inputs (Ford 2017), up to the bundle budget, not a fixed count of
   8 (`tick.rs:22`).
2. **Delta coding** against the previous input in the bundle: a game codec may implement `NetCodec::encode_delta`; the default
   is the existing "same as newer" bit (`bridge.rs`).
3. **Loss-aware redundancy**: when the quality monitor sees non-congestive loss (§ 4.2 item 3), the client sends each input in
   at least `ceil(log(0.001) / log(loss))` datagrams (bounded by the budget), so the residual input loss stays at or under
   0.1 %.

**Built (N207 net-clock-filters, ADR-8312; NetClient path).** Item 1: `InputHistory::ack_through` takes
`LinkFeedback::newest_contiguous_tick`, and an acknowledged input is never sent again. The history keeps the newest
unacknowledged inputs up to `ClientConfig::input_redundancy`, whose default rises from 8 to 32 (`MAX_BUNDLE_INPUTS`, a
whole bundle); a game's explicit lower value stays a cap (keeping every unacknowledged input regardless resent refused
junk inputs in the hostile-input fuzz test). It keeps its inputs across a skipped or repeated tick (a repeat keeps its
first input) and cuts bundles into consecutive runs. Item 2: `NetCodec::encode_delta` / `decode_delta` code each bundle
input after the first against the one before; the default writes the full value (not the prediction path's
same-as-newer bit, which would change every bundle's bytes and need a wire version), so existing codecs send identical
bytes. Item 3: `loss_aware_copies` = `ceil(ln 0.001 / ln loss)` from the worse of the connection's loss fraction and the
server's `upstream_loss` raises a lower configured redundancy (5 %: 3); older bundles of large inputs go only while one
of their inputs has fewer copies (the loss taken as at least 1 %: 2). `net.status` `input_timing` reports
`unacked_inputs` and `input_copies`. Lossy-fat-pipe at redundancy 1: 0.028 % of ticks without input (4.729 % before).

### 6.2 Commands (RPC-style orders, RTS scale)

A command is a tick-stamped reliable message on the inputs stream (stream 1): the server applies it at the stamped tick if it
arrives in time, otherwise at the next unbuilt tick (§ 6.3). Commands larger than a packet are fragmented by the stream (up to
`MAX_RELIABLE_INPUT_BYTES`, 16 KiB, ADR-7896; project-settable to 64 KiB). Per-peer caps: commands per tick (default 16), bytes
per second (default 32 KiB/s), both counted against the hostile-input policy (§ 11.2). RTS-scale games with hundreds of units
per order send orders as commands; games with thousands of simulated units use lockstep (§ 7), where only orders travel.

### 6.3 Never lose a press [D27]

An input or command that misses its tick (arrives after the server consumed that tick) is **merged into the next unbuilt tick**:
button presses and commands are kept, the newest analog values win (DO27's host rule and test). This holds on the NetClient
`TickDriver` path, the prediction path (fixing the dropped or doubled one-step presses of the audit's P2 row, § 9.3) and the
lockstep host. A game can opt out per input field (`NetCodec::merge_late`), default merge.

Built (N209, ADR-8217): on the prediction path the server merges a command for a tick it consumed with a repeat into the next
consumed tick with the new provided method `NetInput::merge_late(late, next)` (once per tick: a duplicate merges nothing); on
the `TickDriver` path `TickDriver::set_late_merge` does the same for the ticks a peer missed, and `NetServer` sets
`NetCodec::merge_late`. The trait cannot see fields, so the defaults differ from "default merge": `NetInput`'s keeps a lone
press (`late != on_missing(late)` and `next` has none: `late`, else `next`), `NetCodec`'s keeps `next` (a late input is used
whole when the next tick has none); a type with presses overrides `merge_late` field by field (OR or add the presses, take
`next`'s analog values), and that override is the per-field opt-out. The input packet limit is a token bucket
(`max_input_packets_per_tick` per tick, `INPUT_BURST_TICKS` = 16 ticks deep), so a flushed burst is kept. The lockstep host is
not on this base.

### 6.4 Server feedback to the client [NEW]

A `LinkFeedback` record on the essential unreliable class, every tick sync (default 250 ms) and immediately after a late or
missing input: newest contiguous input tick received, input arrival margin p50 and p5 over the last window (ticks, signed),
inputs late, inputs missing, inputs dropped as too far ahead (adversarial row 3), upstream loss as the server measured it. The
client steers its lead from the margin (§ 9.2) on both the NetClient path and the prediction path (which already gets a margin
report, `predict.rs:2304-2359`). This is the F2 fix: under downstream bufferbloat the client's own tick estimate is late, and only
the server can say so. A v0.5.x form of it (wire 4) lands first (§ 17 phase 1).

**Built (v0.5.x, N111, ADR-8077; NetClient path):** lane 4 on the essential unreliable class, wire version 4. The record
also carries the server tick it was written on, the input tick of the arrival that set the p5 (for the lead correction of
§ 9.2) and the server's `input_buffer_ticks` (the client clamps its lead to it, so
`set_server_input_buffer_ticks` matters only until the first record). Counters are totals since the join (a lost record loses
nothing); the margin is the input tick minus the server's next tick when the tick's *first* copy arrived (redundant copies are
not samples); immediate records go out at most once per server tick; players that have sent no input and spectators get none.
Upstream loss is the packet-number progress the server saw against packets received over the last 8 records. The prediction
path is not moved onto it yet.

## 7. Lockstep mode (`regia_net::lockstep`) [D27]+[NEW]

For games whose whole simulation is deterministic and too large to replicate (RTS, DO27's live mode). Built on `NetServer`/
`NetClient` and `TickDriver` (with `ready_for`, ADR-7892), over the sealed transport.

### 7.1 Roles

A **host** (a listen-server player or a dedicated observer process) builds one `LockstepFrame` per tick; **seats** send inputs
stamped for `tick + delay` and step only from frames. The host never steps ahead of the frame it sent.

```rust
/// One lockstep tick as the host decided it (spec 120 § 7).
pub struct LockstepFrame<I> {
    /// The tick this frame steps.
    pub tick: u64,
    /// Every seat's input for the tick, sorted by seat; a seat absent this tick is listed with `SeatInput::Absent`.
    pub inputs: Vec<(SeatId, SeatInput<I>)>,
    /// The input delay in force from `delay_from` on (agreed-tick change).
    pub delay: u16,
    /// First tick the `delay` above applies to.
    pub delay_from: u64,
    /// Seats that join or leave at this tick (agreed-tick membership).
    pub membership: Vec<MembershipChange>,
}
```

**Built (v0.7, KI batch 3, ADR-8855):** the dedicated server's observer (`LockstepServer`, ADR-8558) takes the majority
seat's state when the quorum outvotes it (`LockstepObserver::loads_state`/`load_state`,
`LockstepServer::load_observer_state`, `LockstepSeatLink::observer_state`; status `observer.resyncs`, `transfer_tick`)
instead of leaving the quorum for the match; a simulation that cannot load a state keeps the old behavior. The server's
40-seat soak also runs on the real endpoints (`regia_net_soak --test lockstep_server_endpoints`: `NetServer`, 40
`NetClient`s on the impaired network) besides the modelled-stream seat-provenance soak.

### 7.2 Frames travel unreliable, with redundancy [NEW]

DO27 sends frames on its one reliable-ordered channel, so one loss stalls every seat for a fixed 250 ms. Here each frame
datagram (`UnreliableSequenced`) carries the newest frame plus every frame that seat has not acknowledged (seats ack the newest
contiguous frame in their input datagrams), delta-coded per seat input against the previous tick (most inputs repeat, so a
frame costs about one bit per unchanged seat). A frame that does not fit falls back to the lockstep stream. Membership and delay
changes also go on the lockstep stream as the authority, and are repeated in frames until acknowledged.

**Built (v0.6, N224, ADR-8146):** `regia_net::lockstep` (`LockstepHost`, `LockstepSeat`), sans-IO, carried as game
messages on the unreliable and reliable channels by `lockstep::net` (no framing change; the wire version stays 4 until
transport v2). Frames carry the bytes each seat sent and are delta-coded against the previous tick by bytes (one bit
for an unchanged seat, a byte mask otherwise). Redundancy is `FrameRedundancy::Adaptive` by default: the newest frame
plus the unacknowledged frames back to one more than the sequence jump the seat's link produces (two at least), and
every unacknowledged frame once the seat reports a hole; every unacknowledged frame in every datagram (`AllUnacked`, this
section as written) measured 4,513,983 bit/s at 40 seats against 3,642,776 bit/s. The frame is the authority for
membership and delay changes (each frame carries them and none is skipped), so they are not sent a second time on
the stream. Two rules found by review after the first build: the window never starts after the first frame that was
not sent yet (a host that builds several frames between two sends still sends all of them), and a run of frames
handed to the stream begins at the last frame the seat acknowledged or where the last run ended, never at the
window's start (the stream delivers once and in order, so the seat must be able to apply what it carries). A leave
and a join of one seat are never in the same frame: a seat added again while its leave waits for its frame joins on
the frame after it. Under `DelayPolicy::PerSeat` a frame on a tick the shared schedule leaves empty still carries the
inputs of seats that stamped it with their own lead before the raise.

**Amended (v0.6, N224 second pass, ADR-8209):** the default is `FrameRedundancy::Interleaved`. Every datagram carries
the newest frame and the unacknowledged one before it (one lost datagram is repaired by the next); every second
datagram, phased per seat, reaches back over the loss run the seat's link produces plus two, 16 frames at most (a
run up to the link's run is repaired by the first or the second datagram after it); a link that has shown no run of
two never gets the deeper datagram. A deeper datagram that does not fit the budget drops its oldest repeats instead
of using the stream. A seat that receives a datagram starting past its frames reports the hole at once and gets
every unacknowledged frame. `Adaptive` (the window in every datagram) and `AllUnacked` (this section as written) are
one config field away. A seat that is added again while its previous holder has not acknowledged the frame that
lists its leave: the frames that holder still lacks are handed to the application (`HostEvent::SeatHandover`).
Inputs are buffered for a seat from `add_seat` on, so an input that arrives before the frame that lists the join is
in that frame (with a delay of 0 that is the tick 0 input).

### 7.3 Input delay control [D27]+[NEW]

DO27's rule is kept: **raise at once** (every seat's gap ticks pre-filled with empty input), **lower by one tick only after
five calm 2 s windows**, the change agreed on a tick carried in the frame so every seat switches together. Improvements:

- The target delay is budgeted from the measured input arrival distribution at the host, not RTT alone:
  `delay = ceil((p99 one-way input lateness + p99 loss-run recovery) / tick)` where loss-run recovery is the time redundancy
  needs to repair the 99th-percentile loss burst (from the quality monitor's Gilbert-Elliott estimate), bounded by
  `max_delay_ticks` (default 15 at 60 Hz).
- **Equal delay for the host** (`host_input = Delayed`, default for listen hosts): the player host's own input is delayed by the
  same number of ticks, removing DO27's structural host advantage. A game may choose `Immediate`.
- **Delay policy**: `DelayPolicy::Shared` (default; DO27's model and the fair one for competitive play): every seat uses the
  frame delay, which follows the seat that needs the most. `DelayPolicy::PerSeat` (opt-in, co-op and casual): the frame delay
  follows the median seat and a seat whose margin (§ 6.4) is short stamps its inputs further ahead, so only the player on the
  worse link feels it.

**Built (v0.6, N224, ADR-8146):** an input sampled at capture tick `c` (the next tick to step) is stamped
`c + delay(c)`; a change starts at capture tick `frame tick + 1`. Lateness is an input's first clean arrival minus the
frame time of its capture tick; the loss run is the p99 run of the larger direction from the module's own datagram
sequences (a geometric fit, capped by the longest run seen) until `quality.rs` supplies it. Gap ticks of a raise are
listed `Absent` for every seat and not counted as missing; a lower skips the captures whose tick is taken. Measured:
bad-wifi, 4 seats, 5 minutes: delay 9 ticks, at most 19 of 17,991 ticks without
a seat's input. `PerSeat` is built in its basic form (the seat's own lead from the host's `LinkFeedback` record).

**Amended (v0.6, N224 second pass, ADR-8209):** the loss term is the **loss delay tail**, not the 99th-percentile
loss run:

`delay = ceil(p99 one-way input lateness / tick) + loss delay`, where the loss delay is the smallest `k` such that
at most 0.5 % of a direction's datagrams are delayed more than `k` send intervals by loss (the larger of the two
directions).

A datagram's delay by loss is the number of consecutive losses that start with it, so under the geometric fit
`P(delay > k) = p * q^k`, with `p` the link's loss rate over the last minute and `q` the chance that a run continues;
never above the longest run seen. The 99th-percentile run is the tail among lost datagrams only (`q^k`): it
budgeted a link that loses one datagram in a thousand like one that loses one in ten. Half a percent each way
leaves about one input in a hundred late for loss when both directions lose. Measured on the same seeds, the mean
delay over the run before and after (and the delay at its end): bad-wifi, 4 seats, 5 minutes: 10.05 to 7.90 ticks
(9 to 7), at most 0.27 % of a seat's ticks without its input (was 0.11 %; the bar is 2 %); lockstep-mix, 40
seats: 14.04 to 12.59 ticks at 60 s (14 to 12) and 13.23 to 12.11 ticks over 5 minutes (12 to 11), at most 0.30 %
(was 0.20 %); one lost datagram still stalls a seat one tick in every run.

**Amended (v0.6, ADR-8656):** under `DelayPolicy::Shared` the loss term is the host's own measurement only: the delay
tail of the seat's **uplink** (its inputs datagram sequence at the host). The downlink run a seat reports, and an
extra lead it reports, are not host measurements, and under `Shared` the largest need is every seat's delay: one
datagram reporting a 32-datagram run moved every seat from 4 ticks to 15. The downlink still counts where the host sees
it (a late frame opens the seat's capture late, so it is in the measured lateness) and in that seat's own frame
redundancy. Under `PerSeat` both reports count in the seat's own need, because the frame delay follows the lower
median need, which a minority of seats cannot move.

- **Late stamping is flagged** [NEW]. Equal delay for the host does not stop a modified client from sampling its
  input later than its capture tick wherever the delay leaves room. An honest seat sends the input of a capture
  tick in the datagram that first acknowledges the frame that opened it, so the hold between that acknowledgement
  and the input's first arrival is zero on any link. The host keeps the hold and the margin (deadline minus
  arrival) of each seat's clean first arrivals over ten seconds and flags a seat when, over at least 240 inputs,
  nine in ten were held two ticks or more and the median margin is no larger than that hold; with the transport's
  round trip fed in, the time the seat's frame acknowledgements trail it counts as hold. The flag is
  `HostEvent::LateStamp` and `net.lockstep` (`late_stamp_suspects`, and the hold and margin per seat). Nobody is
  removed for it. A client that delays every packet it sends, the transport's acknowledgements included, is a slow
  uplink to every measurement: that is the limit.

### 7.4 Silent seats, leaves, joins, start grace [D27]

A silent seat never blocks the frame past its deadline: it is marked `Absent` (the game's `MissingInputPolicy`: `Empty`,
`RepeatAnalog` with presses cleared, or `Hold`), and its late inputs merge into the next unbuilt frame (§ 6.3). A leave applies on
the same tick everywhere (membership in the frame). Loaders get a start grace (default 30 s) before silence counts. Seats are
removed only for silence past the session timeout (§ 10.1).

**Late join and reconnect in lockstep** [D27 `reconnect.rs` design]: the host sends the game's serialized state at a recent
tick K (bulk stream) plus every frame since K; the seat runs the frames to catch up (faster than real time, bounded by a CPU
budget per frame), then joins at an agreed tick. A gap in the frame log refuses the catch-up with a clear reason.

**Built (v0.6, N225, ADR-8212):** `LockstepConfig::missing_input` (`MissingInputPolicy`, default `Empty`) lists a
silent member `SeatInput::Held` with its last input (`Hold`) or that with its presses cleared (`RepeatAnalog`, via
the game's `InputRules`); the wire still carries the seat as absent and every participant derives the held value
from the frame chain. Gap ticks stay empty for every seat. The host remembers the ticks it built without a member's
own input and merges that input, when it arrives, into the next non-gap frame, once (`InputRules::merge_late`;
without rules the seat's own input wins and the newest late input fills a tick it has none for). A seat asks to
leave with `LockstepSeat::leave`; `LockstepHost::end_match` makes the next frame the last (`LockstepFrame::match_end`).
Silence is judged once per frame by `LockstepConfig::timeout`, the session's `TimeoutPolicy` (§ 10.1): interruption
events, removal with reason `timeout`, the start grace from the seat's admission until it is first heard. The
catch-up is `LockstepHost::catch_up(seat, state_tick, state)` on the lockstep stream (the frame chain's delta base
after the tick, the state in pieces, the frames since); `LockstepSeat::catching_up` steps at most
`catchup_steps_per_frame` (default 8) frames per application frame and joins on the frame after it reports the live
edge (a member resumes in place); `lockstep_catchup_gap` when the log does not hold the frames. The seat id survives
a reconnect through `LockstepHost::resume_token` / `resume`, a keyed hash under a per-host key, valid while the seat
is a member or for `resume_window_ms` (120 s) after a timeout. The real-transport check pins a 12 s outage on the
`flappy` link (past the 10 s frame log, inside the 20 s removal timeout). The named `flappy` profile's long outage
stays the removal timeout plus 2 s (ADR-8209). No framing change.

### 7.5 Bandwidth [NEW]

Host upload is `seats x frame size`; with per-seat delta coding a 40-seat frame is about one byte per changed seat plus headers.
Bar (§ 15): 40 seats at DO27's recorded input mix <= **4 Mbit/s host upload** (DO27 measured 11.1 Mbit/s on its live path). Games
that need more than about 64 seats on a home uplink should use replication.

**Built (v0.6, N224, ADR-8146):** `cargo test -p regia_net_soak --test lockstep`: `lockstep-mix` at 40 seats,
3,642,776 bit/s host upload with IP and UDP headers (2,400 datagrams/s), order-driven input mix
(`InputMix::ORDERS`, the stand-in for DO27's recorded mix; an action mix where half the seats move their aim on any
tick is in the lane report). That is the mean over the run, which is what the bar is judged on; the busiest whole
second of the same run is 4,170,848 bit/s.

**Amended (v0.6, N224 second pass, ADR-8209): the bar is a per-second ceiling.** A player feels the worst seconds,
not the mean. Over a 5-minute run, the host's upload in the **95th-percentile whole second is at most 4.0 Mbit/s
and in the busiest second at most 4.4 Mbit/s**, IP and UDP headers included. It is stated per input mix:

| Mix | Seats the bar holds for | Measured over 5 minutes (mean / 95th-percentile second / busiest second, bit/s) |
|---|---|---|
| order-driven (`InputMix::ORDERS`: a seat changes its input on about 13 % of its ticks; the stand-in for DO27's recorded mix) | **40** | 40 seats: 3,333,010 / 3,591,912 / 3,755,680 |
| action (`InputMix::ACTION`: every seat moves its aim on half of its ticks) | **24** (tested and gated; on the default seed it still holds at 28 and 30 seats and not at 32) | 16 seats: 1,372,373 / 1,427,288 / 1,497,096; 24 seats: 2,524,190 / 2,641,096 / 2,733,880; 28 seats: 3,232,642 / 3,373,936 / 3,561,216; 30 seats: 3,592,113 / 3,751,032 / 3,923,368; 32 seats: 4,014,040 / 4,189,944 / 4,372,096 (over); 40 seats: 5,804,388 / 6,068,128 / 6,289,024 (over) |

Before the amendment the order-driven mix at 40 seats measured 3,664,500 / 4,038,776 / 4,292,416 (19 of 297 seconds
over 4.0 Mbit/s): the mean held and the 95th-percentile second did not. The difference is interleaved redundancy
(§ 7.2). `cargo test -p regia_net_soak --test lockstep` judges both mixes; `cargo xtask net-soak` runs them as the
`lockstep-mix` and `lockstep-mix+action` profiles. `lockstep-mix+bots` (ADR-8409) is a third mix: a heading held
60 to 180 frames, so the bar uses the orders seat cap (40). Its gate row is 40 seats for 600 s.

### 7.6 Desync detection, diagnosis and recovery (`regia_net::desync`) [D27]+[NEW]

```rust
/// Game-supplied hashing of its deterministic state (spec 120 § 7.6).
pub trait DesyncHash {
    /// One 64-bit hash of the whole state after stepping `tick` (cheap; sent every `hash_every` ticks).
    fn world_hash(&self, tick: u64) -> u64;
    /// Named per-subsystem hashes for the probe (at most 64), folded after every step into a ring.
    fn subsystem_hashes(&self, tick: u64, out: &mut Vec<(&'static str, u64)>);
}
```

- Every seat sends `world_hash` every `hash_every` ticks (default 30) in its input datagrams; per-subsystem hashes are kept in a
  ring of 240 ticks and travel only on a probe [D27].
- **Corroborated verdict** [NEW] (DO27 ends the match on one differing report, so one hostile client can end any match): a
  verdict needs the host's own hash plus either a dedicated observer's hash or a majority of seats. A seat in the minority is
  `Diverged`: it is **resynced** through the late-join path (§ 7.4), counted, and after `max_resyncs` (default 2) in a match
  removed with `desync`; the match continues for everyone else. A host in the minority (seats agree among themselves against
  the host, with an observer or a 2/3 majority) is a host fault: the match migrates (§ 10.5) or ends with `host_desync`.
- **Probe** [D27]: on the first split the host asks the diverged seats and one agreeing seat for their subsystem rows around the
  split; the verdict names the first diverged subsystem and the last agreed tick.
- **Desync bundle** [D27]: inputs since the last agreed snapshot, the snapshot, and the rows, written by the host (and the
  diverged seat when it can) as `*.desync.ron` (new format, version 1, § 12.4); `regia net desync-replay <bundle>` re-runs the
  game step headless and names the first diverging tick and subsystem; peers are walked in sorted order on both paths (the
  study found DO27's offline replay walks a hash map while its live path sorts).
- **Injection hook** for tests (`DesyncInject`, test builds and the agent protocol only): flips one subsystem at a chosen tick;
  a test asserts it is named by the next hash tick.

**Built in part (v0.6, N224, ADR-8146; the verdict rule only):** `regia_net::lockstep` tallies checkpoint hashes by
quorum. One vote per simulation (a listen host votes as its seat); a strict majority of the votes cast is the
reference and every other voter, the host included, is ordered to take a majority voter's state; a seat is removed
after `max_resyncs`, a host past it raises `HostFault` (the migration seam) and no seat is removed for it. Voters
outside the majority are first asked to replay from the last agreed checkpoint and vote again. Without a strict
majority nobody is removed: `TiePolicy::HostGroup` or `EndMatch` (`desync_unresolved`). The host probes per-tick world
hashes to name the first diverged tick. A vote is only taken for a tick the host has built, and when 64 checkpoints
wait for votes a new one is not opened (the oldest is still tallied at its deadline; `vote_deadline_ticks` is held
to 24 checkpoint intervals so the settings stay clear of that). `DesyncHash`, the per-subsystem probe, the bundle,
the replay command and the agent methods of this section are not built.

**Built (v0.6, N226, ADR-8211; detect and probe):** `regia_net::desync` holds `DesyncHash` (this section's
signature; it becomes a re-export of the detsim kit's identical trait, ADR-8047, when both are in one line), the
240-tick `SubsystemRing`, `locate`, `DesyncReport` and `DesyncInject`. Seats and the host hash with `record_state`
(rows kept, world hash reported as the vote) and `record_replayed` (a replayed tick: rows and the probe's per-tick
hash replaced, no vote; `report_replayed_hash` for plain hashes, ADR-10147). The
probe runs in two stages: the per-tick world hashes of every diverged voter (at most 8) and the verdict's source name
each voter's first diverged tick, then only the rows at that tick and the tick before it are pulled (two rows instead
of the window), compared by subsystem name; `HostEvent::DesyncProbed` and `net.lockstep`'s `last_desync` name the
first diverged subsystem and the last agreed tick. Both ends keep the probe window's rows as they were when the probe
started, because the transfer ordered by the same verdict rewrites them. `DesyncInject` is consulted in the game's
step (no agent method injects; a game exposes it through its own test hooks). The verdict rule is ADR-8146's quorum,
not this section's "host plus observer or majority" wording (ADR-8211 § 5 has the difference). The bundle and
`regia net desync-replay` are not built (N227).

**Built (v0.6, N227, ADR-8455; bundle, replay, injection):** `DesyncBundle` (`*.desync.ron` v1) from the host
(`from_host`: frame log, the newest probe's rows by voter merged with the host's own ring, the live report) or from a
diverged seat on `SeatEvent::Resync` (`from_seat`); the snapshot is the game's own bytes at the verdict's last agreed
checkpoint (or the match start). Refinements: inputs are written as `Input::to_bytes` of the logged decoded value (the
public frame logs keep decoded inputs; the replay decodes to the same value); the replay hook is `DesyncReplay`
(`identity`, `load`, `step` plus `DesyncHash`) in a `ReplayRegistry` by game id, and a bundle of another game or game
version (protocol version or protocol id) is refused with both versions named; `regia net desync-replay` registers the
engine's sample hook only, since the `regia` binary cannot link a game's step: a game calls the same code from its own
tool binary. The replay is the reference and the earliest tick any peer differs from it is the verdict, with
`matches_live` against the live report. `DesyncInject` reaches a game through `net.desync_inject` (development and test
builds, `desync_inject_disabled` otherwise) and `NetAgentState::take_desync_inject`; `net.desync` shows the newest
report, the probe rows, the bundle and the replay verdict.

**Built (v0.7, NL-02 lockstep-replay-audit, ADR-8513; landed by ADR-8892):** a whole match is kept as a `.lsreplay` v1
(`regia_net::lockstep::replay`, format batch 2): Fulls of the game's own state bytes at the tier's rate, every agreed frame,
checkpoint hashes, peer commands and an abort record, under a per-tier byte cap. `audit` re-runs it through the same
`DesyncReplay` hook from each Full and names the first diverging tick and subsystems (`regia net replay-audit`,
`net.replay_audit`); `ReplayLibrary` lists, rotates, imports and exports. Refinement on landing: the frame record carries no
match end (`LockstepFrame::match_end` is read back `false`; a step never reads it, and the end of the file ends the match).

### 7.7 Rollback lockstep (small matches) [PUB]

For 2-8 seat action games (fighting, sports): seats predict missing remote inputs (repeat last), run ahead with `RollbackLog`
(§ 9.5), and roll back when the real input arrives (the GGPO-style model, public documentation). Same frames and delay control,
with `delay` typically 1-3 ticks and a rollback window of up to 8 ticks. Phase 3 (§ 17).

**Built (v0.7, N304, ADR-8361):** `LockstepConfig::mode = LockstepMode::Rollback { max_rollback_ticks }` (window
1..=8; preset `LockstepConfig::rollback(hz)`: delay 2, kept between 1 and 3 by the § 7.3 controller,
`MissingInputPolicy::RepeatAnalog`) and `lockstep::RollbackRunner` over a `RollbackLog` (§ 9.5) of the window plus two
ticks; the game's state implements `RollbackGame` (`step(&LockstepFrame)`, `hash()`). Same frames and wire (no
framing change). Three refinements the build needed: (1) the host holds a due frame for a live seat's input for up to
**twice** the window (a seat counts as live from its admission until it has been silent for the delay window), so the
other seats predict through the first window and wait at its edge for the rest and an input is waited for rather than
dropped; with a hold of one window a transatlantic seat at delay 1 lost its first inputs and the confirmed state left
the reference; (2) a delay change starts `window + 1` ticks after the frame that carries it (a rollback seat may have
sampled up to the window past its frames); (3) the seat's clock starts at the first frame's arrival (that tick is due
then) and is steered by the host's input margins, measured under rollback against the host's frame clock rather than
the frame it holds: a median margin below zero moves the clock ahead at once by the deficit, one above two ticks for
five calm seconds moves it back one tick, at most the window either way (`clock_lead_ticks`); every seat aligned to the
host's frame clock keeps the frame advantage balanced, and the hold is left for jitter. A seat waits at the window's
edge instead of predicting further (`window_stalls`). Steering on the 5th-percentile margin instead (the client input
lead's rule) halved the inputs merged late on `bad-wifi` but tripled the replayed ticks and the stalls; no sync at all
stalled more and dropped more inputs on `bad-wifi`. A predicted tick is the seat's own input as sampled, every other seat's last input with its presses
cleared, every seat absent on a gap tick. Measured (soak kit, real transport, virtual time, 60 s): two seats on
`transatlantic`, delay 1 / 2 / 3: deepest rollback 8 / 7 / 7 ticks, at most 14 / 7 / 0 window stalls per seat in
3,750 application frames, about 440 rollbacks per 3,580 ticks, every input in its frame, the confirmed state equal to
the lockstep reference stepped offline from the input scripts at the last common tick; four seats on `bad-wifi` for 10
minutes with the preset: 0 desyncs, 0 resyncs, in step at tick 35,989, deepest rollback 8 ticks, 357-442 window stalls
per seat (about 1 % of 37,500 application frames), 3-38 inputs per seat held and merged late after the two-window hold
(without the clock sync: 378-562 stalls, 4-50 inputs).

## 8. State replication

### 8.1 Kept and fixed [REG]

Acked per-entity baselines with a 256-bit ack window and expiry to full state; priority accumulators; per-client byte budgets;
incremental grid relevance with hysteresis. Fixed: every record (spawns and forced records too) obeys the byte test of the
budget, and debt carries over (adversarial row 4); the budget is `min(project rate, controller allowance)` (§ 4.2 item 9); an
entity that grows past the spawn limit is never frozen (R5: updates and spawns share the limit, oversize records go to the
replication-control stream with an RTT-based in-flight timeout); the standalone `SnapshotEncoder` never moves `current()`
backward on a reordered packet and never replaces a world with a partial full packet after ring overflow (R1).

Built (N108, ADR-7983): a late or repeated packet is kept as a baseline (not refused) but never moves `current()`,
`last_seq()` or `latest_baseline()`; the decoder merges a budget-limited full packet (`is_rebasing()` until the first complete
packet) rather than the encoder overrunning its budget; baselines on both sides are copy-on-write. Two refinements: a full
packet more than `capacity()` sequences behind is a stream restart (plus an explicit `SnapshotDecoder::reset()`), and while
re-basing a server-side removal shows only at the first complete packet.

**Built (N212, ADR-8419).** One 16 KiB cap for spawn and update records (chunking rejected for now: it needs a wire layout for
partial versions); a refused record of a held copy withdraws the copy (despawn or out-of-scope notice), reported once per crossing
of the cap, and the copy returns when it fits; a refused slot is not encoded again until its state version changes. The in-flight
timeout is `ceil(srtt + max(4 rttvar, 1)) + 2` ticks from state-frame acknowledgments (RFC 6298 gains; 30 before the first sample);
the oversize slot frees on its own acknowledgment or after four timeouts; the state log keeps frames younger than two timeouts
(at least 120 ticks) above 256, up to 4,096. Every record obeys the byte test (one first record may overshoot a tick with budget
left) and the debt carries over in full. Known limit: the client's 64-version ring expires the baselines of an entity that changes
every tick on a round trip over 64 ticks. R1 (`SnapshotEncoder`) was not part of this lane.

**Built (v07-net-state-ring-reorder, ADR-10051; wire 6).** The known limit above is lifted: `STATE_VERSION_RING` is 512, sized
from `rtt-extreme` (a 3 s round trip is 180 versions at 60 Hz and 384 at 128 Hz for an entity that changes every tick) plus the
acknowledgments a 20 % loss delays, so a round trip of up to 4 s at 128 Hz (8.5 s at 60 Hz) keeps deltas. A client does not
store by that count: it lets go of the versions older than a baseline the server named once a frame of the entity 8 ticks newer
has arrived (`STATE_BASE_REORDER_TICKS`; the server's baseline only moves forward), so a ring holds one round trip of versions
plus those of 8 to 16 ticks (measured at the default send rate: 9 to 10 on a 12 ms path, 24 to 30 on `high-rtt`, 87 to 90 on the
`rtt-extreme` peer; 64 everywhere before). On `rtt-extreme` at 8 and 32 clients the slow peer's downstream went from 370-421 to
81-82 kbit/s and its staleness p99 beyond the path from 11-15 to 7-8 ticks; `high-rtt` at 60 Hz is unchanged (58.4 kbit/s). A frame reordered past that margin naming a version let go is taken
back from the acknowledgment and reads as lost; it asks for no resync. No frame layout changed. R1 was already built by N108
(first Built note above); the lane's audit of it is in its report.

### 8.2 Quantized transforms and error-gated deltas [D27]+[PUB]

Default transform record: position quantized to a project-set precision (default 1/1024 m inside a world cell, 20-bit per axis
with the cell id on change), rotation as smallest-three quaternion at 3 x 10 bits (Fiedler 2015), velocity at 3 x 12 bits when
present; a record is sent only when the receiver's extrapolation from the acked baseline errs by more than the gate (default
2 cm / 0.5 deg) [D27 18-byte record and 2 cm gate]. Bar: <= 10 bytes per moving entity update at default precision (today 23-35).
Old-protocol clients are refused by the wire version, not misread.

**Built (N213, ADR-8467; protocol 4).** The precision is the profile's `position_step_m` (default 1 mm, so a cell is
`2^20` steps = 1,048.576 m; the step rides in one bit when it is the default); a child, a profile without a step and a
position past `2^31` steps stay raw `f32`, as does a velocity past +-64 m/s. Updates on the state channel are deltas against
the acknowledged baseline's integers (change bit per part and component, zigzag residual classes of 4/8/14 bits or a
varint), and record framing shrank (`ago` and empty-entry codes). Old peers are refused by the protocol hash (a
transform-record revision row), not the wire version: wire 5 is the line's open bump and the framing below replication did
not change. Walking crowd: 9.03 B per moving entity update (base 35.76), 142 walkers per send tick on the default budget
(base 45). Not built: the 2 cm / 0.5 deg extrapolation gate (it needs client-side extrapolation between records, and it
conflicts with the precision bar unless it is a profile option) and velocity-based prediction of the position residual.

**Built (ADR-8522; transform record revision 3).** The gate is the profile option `extrapolation_gate_m` (`.netprofile.ron`
v2, default off so the precision bar holds; the rotation half is 0.5 deg). On the acknowledged state channel a gated
entity's transform-only change is held back while the client's extrapolation of every record it may be showing (the
update records on their way since the acknowledged baseline, or that baseline) is within the gate. Records carry a steady
bit: a steady proxy extrapolates past `max_extrapolate_ms` and never starves, and a copy without interpolation moves along
its record (`NetSteady`). A position delta whose baseline has a velocity may code its residual against the baseline moved
along it (one bit; the predictor runs at a fixed 60 Hz, so it never depends on the two sides' clocks). Walking crowd with
the gate off: 7.25 B per moving entity update (was 9.03). With a 2 cm gate on good-wifi: 1.3 kB/s instead of 21.8, mean
shown error 0.8 cm instead of 1.2, worst 11.6 cm instead of 8.0 (a turn shows until its record lands). Known limit: the
gate trusts `NetVelocity`. Table: `docs/perf/net-extrapolation-gate.md`.

### 8.3 Priority and rate tiers [D27]+[REG]

Priority = accumulator x (relevance weight) with game inputs (DO27's combat-heat term is one: recent damage dealt or taken near
the viewer). **Distance rate tiers** per profile [D27]: near (default < 150 m) every send, mid (150-600 m) every third send, far
(radar) at 2 Hz as a reduced record (position only, 5 bytes), hidden never.

**Built (N214, ADR-8031).** `NetProfile.rate_tiers: Option<RateTiers>` (`.netprofile.ron` v2, `regia upgrade-project`
rewrites v1's version number) gates *updates*: near every send, mid every `mid_every`-th send, far at `far_hz` rounded
up to whole send intervals; owned, forced (`max_interval_ticks`) and spawn records never wait, and a held-back entity
keeps accumulating priority. The far tier's reduced 5-byte record is not built: it is a record-format change that
belongs with the quantized-record work of § 8.2 (wire bump of that line); a far entity sends its normal delta at the
far rate. Combat heat is `NetCombatHeat` (server-only component) raised by `net_note_combat`: the gain is multiplied
by `1 + heat × max(0, 1 − d / radius_m)` (default radius 50 m, half-life 2 s, heat clamped to 10).

**Designed, not built (wire 6 window, ADR-10052 (1)).** The far record is position only, 40 bits, for an entity whose
profile has `rate_tiers`: `x u15 | z u15 | y u10`, the offset inside the entity's current cell (`POSITION_CELL_BITS` =
20) shifted right by 5 on x and z (32 steps: 3.2 cm at the default 1 mm step) and by 10 on y (1.02 m), sent only while
the receiver's acknowledged baseline is in the same cell (a cell change sends the normal record). Both sides keep the
coarse integers as the new baseline (no drift); rotation and velocity keep the baseline's; an owned, forced or spawn
record is never far. It needs a lead bit on such a profile's updates, `TRANSFORM_RECORD_REVISION` 4, a codec and fuzz
seed, and a bandwidth row at 40 clients against today's far tier (the normal delta at the far rate). It did not fit the
window lane; it lands before the 11-28 freeze or waits for the next wire step.

**Built (wire 6 window, ADR-10124, which amends ADR-10052 (1); transform record revision 4).** The far record is state
record kind 3 (`far := base_ago | far position | entries`), so no other record pays a bit and the client needs no
profile. The far position is `1 | x u15 | z u15 | y u10` or, when shorter, `0 |` three residuals against the
baseline's coarse position. A coarse integer is the offset in the cell rounded to the nearest `2^5` (x, z) or `2^10`
(y) steps: the shown position is within 1.6 cm and 0.512 m of the server's at the 1 mm step, and a position on the
coarse grid is exact. It is a function of the server's position alone, so far records agree under loss and
reordering. A far version is never a transform-delta baseline: the exact record after it is absolute, and an exact
record of the same version replaces the far form on the client in either arrival order. Rotation and scale stay as
shown; the velocity is dropped (the proxy infers it, § 9.7), and a proxy's `NetVelocity` goes with it until an exact
record brings the current one (ADR-10215). A far-tier update (not owned, forced, teleported, gated
or re-parented; acknowledged state channel; same cell, grid and mode as its baseline) takes the far form when it is
at least a byte shorter than the exact record (any shorter once it is held in its far form); with a far baseline a record is due only when the coarse position or a component
changed. The first record after a copy leaves the far tier restores its exact transform, even if the entity did not
change. **Settle (ADR-10199):** a version that has stood for `FAR_SETTLE_SECONDS` (2 s) never takes the far form, so
a far copy whose entity came to rest gets its exact transform once, in the far tier, and then leaves the gather's
walk like any copy that is even with the server. `ServerReplication::set_far_records(false)` keeps exact records. Bandwidth, 40 clients on a 128-walker crowd:
`docs/perf/net-far-record.md`. Not covered: the reliable tick stream, gated entities, `external_replication`
(game-encoded records).

### 8.4 Interest management at scale [REG]+[NEW]

Each relevance query uses its own profile's radius (today the largest radius of any profile, `relevance.rs:1549-1562`); held
entities are re-evaluated only when they or the viewer moved a cell; a spawn marks only the cells it lands in dirty (today every
connection rescans, `replication.rs:3288-3297`); per-tick per-connection allocations are pooled. Bars (§ 15): the 128 x 20k
relevance bench with a 1 km profile and 64 spawns per tick within mean **and** p99 budgets.

**Built (N214, ADR-8031).** The grid keeps one band per spatial radius and each band is queried with its own radius
(newcomers only need `radius`; held entities are re-checked through the held list). A held entity keeps its verdict
when neither it nor the connection's viewpoints moved, and it is plain, spatial and not lingering: exact, not "moved a
cell". A spawn is relevance input for its own slot only (no full pass); a despawn removes its slot; a new entity
resolves its own owner, and only a despawned `NetOwnerLink` target or a changed owner input forces the full owner
resolve, after which only the copies whose owner flipped are visited. Spawn candidates are a connection's gained slots
plus its owed list. Per-connection scratch (lost/gained lists, the targeted-message recipients) is reused, and the
per-target unreliable cap is a counter (was a scan of the queue per message per peer).

### 8.5 Resume for replicated games

See § 10.3: the replication peer is kept dormant (entities, ownership, interest state) through the grace window; baselines are
dropped so the first frame after resume is a full state.

Built (N216, ADR-8298): as designed; see the § 10.3 "Built (N216)" note for the refinements.

### 8.6 Replication for a sim outside the ECS (DO27 need "net-external-sim-replication") [NEW]

`ExternalReplication<K>` (core, no ECS): the game supplies entity keys, positions for relevance, a priority function and record
codecs; the engine provides relevance (the same grid), priority accumulators, byte budgets from the controller, acked baselines
and per-client `SnapshotEncoder` streams, plus `ExternalInterpolator` for the client side. It is the non-ECS twin of § 8.1-8.4 and
shares their code below the ECS adapters.

**Built (N230, ADR-8186).** `regia_net::external_replication` (core: tested with `--no-default-features`). Shared code:
the new core module `regia_net::interest` (the banded `InterestGrid`, `RelevanceShape`, `RateTiers`, the hysteresis rule
`spatial_limit`, all moved out of `relevance.rs`/`profile.rs`, old paths re-exported) and the now-core `send` (priority
gain, carry-over budget). Refinement: a tier hold cannot be carried by the codec's baseline chain alone (with an ack lag
of L packets the chain splits into L interleaved chains and a held entity never settles), so delivery is per entity: the
client's `ExternalAck` names its newest baseline plus a 64-packet bitfield of the packets it applied, the server keeps
per held entity the packet and the state fingerprint of its last record, and `SnapshotEncoder::encode_selecting` defers
or forces records (a forced unchanged record re-sends the narrowest field: valid on every decoder, no wire change). The
client shows the newest applied record per entity (`ExternalInterpolator::latest`/`sample`), not the chain's world.
Budget: `min(project rate, declared rate, controller allowance)`, floored at `MIN_CLIENT_RATE`, capped per packet at
`max_packet_bytes`. Acceptance test: 2,000 entities, 16 clients, bad-wifi both ways.

**Built (KI-NT-012, ADR-8892).** The engine agent method: the game publishes `ExternalReplication::stats()` under a label
into the `NetExternalStats` resource (`publish_stats`, at most 16 labels, the tick and a publish count kept) and
`net.external_stats` returns the snapshots as serde writes them plus totals over the clients. The engine still never
finds the replication itself: it lives in the game's process.

## 9. Time

### 9.1 Clock sync [PUB]

The NetClient tick estimate uses an NTP-style clock filter (RFC 5905 § 10): of the last 8 tick-sync samples, the one with the
smallest round trip sets the offset; the estimate is **never moved backward** (it slews: up to 5 % faster or slower until it
meets the target) and steps forward only on a jump over 8 ticks (a resync, counted). One RTT spike affects at most the 8-sample
window, not 64 (`interp.rs:182-223`). Bars: 0 backward steps over 10 minutes on good-wifi, bad-wifi, rtt-step and clock-drift.

**Built (v0.5.x, N111, ADR-8077):** a tick sync has no request, so its round trip is unknown; the filter takes the sample with
the largest `tick * tick_ms - arrival` (the smallest one-way delay, which is what the round-trip choice of RFC 5905 § 10
selects) and adds half the smoothed round trip, bounded to the input horizon. The first 8 syncs after a connect anchor the
clock (forward steps there are not counted as resyncs: the accept carries no delay estimate).

**Built (v0.7, v07-net-staleness-rtt, ADR-10096):** the accept anchors the clock with the handshake's round trip. Each
handshake leg is counted from the first send of a flight to the last `receive` that did not yet hold its answer (a
lower bound of the path's round trip however long the client's frames are); the connect keeps its shortest leg (a lost
first copy makes a leg longer by whole resend intervals), takes one tick off for the server's wait, and uses half of it
as the one-way delay until the connection has a round-trip sample of its own. Before, the estimate sat the whole
one-way delay behind the server until that first sample (one round trip after the accept: 45 ticks for 3 s on
`rtt-extreme`, 7 to 10 ticks for 0.7 s on `high-rtt`), every input of the stretch was stamped late, and the sample then
stepped the clock (a counted resync when it came back after the filter's warm-up of 8 syncs, about 2 s, as on
`rtt-extreme`).

### 9.2 Input lead [PUB]+[NEW]

Both paths steer the lead from the server-reported margin (§ 6.4): target margin p5 >= 1 tick; raise the lead at once by the
deficit, lower by one tick after a calm window (the same rule as lockstep, § 7.3). The lead is clamped to the server's input
buffer (adversarial row 3). Startup: the first lead comes from the handshake RTT plus jitter, correct within 2 s.

**Built (v0.5.x, N111, ADR-8077; NetClient path):** `NetClient::input_lead_ticks()` and `due_input_ticks(now)`. A record's p5
covers two seconds of inputs, some stamped with an older lead, so the controller adds how much further ahead the client stamps
now than it did for the input that set the p5 (the record names its tick) before taking the deficit; without that one late
spell raised the lead once per record. "Calm" is every record for 5 s showing a corrected p5 of at least 2 ticks and no new late input. The lead
stays at least 1 and two ticks inside the server's input buffer. Startup lead before the first record: two ticks plus two
measured jitters (the handshake round trip is not used; the first record arrives within about 250 ms plus one way).

**Built (v0.7, v07-net-staleness-rtt, ADR-10096):** the startup lead is this section's: two ticks, two measured jitters and
the path's one-way delay as the handshake of § 9.1 measured it (half its shortest round trip with one tick of the server's
wait taken off; not the connection's round trip, which grows with every queue the first seconds build: a startup lead that
followed it kept `congested` over its bottleneck past the fifth second in 10 of 64 seeds). The estimate is the server's tick now, so the way to the server is the lead's to cover;
with a startup lead of two ticks every input of a path longer than that was late until the first record had come back and
raised the lead, a round trip and a one-way delay after the connect. Measured at 30 Hz
(crates/regia_net/tests/connect_clock.rs): the server has the client's input in time 1.56 s after the connect on a 1,500 ms
path (was 4.73 s), 0.35 to 0.43 s at 325 ms with 20 ms of jitter (was 1.06 s), 0.09 s at 60 ms (was 0.25 s). The lead the
first record then settles on is the path's and the margin (48 ticks at 1,500 ms, 13 at 325 ms, 5 at 60 ms): with the clock
a one-way delay behind, that record read a deficit of the path twice and raised the lead to 62 (the buffer's cap), 22 and
7, which then came down one tick per five calm seconds. A link without delay starts at two ticks, as before.

**Built (v0.7, integ-v07-net, ADR-10231):** a lowering rests on a margin the server measured: "calm" is every record for
5 s showing a measured and a corrected p5 of at least 2 ticks. The correction adds how much further ahead the client stamps
now, and the stamp offset follows the client's tick estimate, which wanders with a queue on the way down; on asym-bufferbloat
five seconds of a spare tick that was only expected lowered the lead from 18 to 17 and the next measured margin raised it
again. A deficit still reads the corrected margin, and a lead raised too far still comes down one tick per calm window.

### 9.3 Dilation and input capture [PUB]+[D27]

Time dilation (Ford 2017) uses a fractional tick accumulator (the client runs at `1 +- 2 %` tick rate), never adding or skipping
whole ticks per step (`predict.rs:2259-2267` today). Input is captured every rendered frame and accumulated into the tick being
built (presses OR-ed, analog latest), so a 0-tick frame keeps its press for the next tick and a 2-tick frame does not repeat a
press. With § 6.3 this closes the P2 row: every one-step press is applied exactly once.

Built (N209, ADR-8217): `ClientPrediction` keeps the continuous time as a lag of `0..1` ticks behind the tick
(`dilation_lag()`); each fixed step advances it `1 -+ max_dilation` while the margin is off by a tick or more, else 1 plus a
catch-up, and runs the ticks that start in the step. Predicted entities are drawn at the continuous time through a dilation
part of `NetVisualOffset` (between the last two predicted ticks), so the own pawn moves `1 -+ max_dilation` ticks per frame.
The default `max_dilation` stays 5 % (not the 2 % above). The prediction path's "frame" is its fixed step: the game writes
`InputCommand` there, the step latches it (merging a 0-tick step's latch with `merge_late`), the first tick takes it and a
second tick gets `on_missing`. A tick re-run under a backward resync's hold (never sent) keeps the press latched for the first
tick that is sent. Evidence: `regia_net` tests `dilation_input_capture` (over 1,000 dilation events on bad-wifi and
clock-drift shapes, each press once on client and server; per-frame displacement 0.5-1.5x a tick's step) and
`late_input_merge` (the DO27 scenario).

### 9.4 Prediction at high RTT [PUB]

`max_resim_ticks` scales with the measured RTT (the history covers `RTT + lead + 2 ticks`, up to 64 ticks at 60 Hz = 1,067 ms,
project-settable to 128). Over the cap, the client snaps to the server state and **continues predicting** from it (today it clears
history and replays nothing, so prediction switches off at about 400 ms RTT, `predict.rs:3011-3012`). Corrections over the snap
threshold are spread by the visual offset of § 9.5. Bar: at 450 and 650 ms RTT prediction stays on (at most one reset after start) and
the own-pawn correction p95 is within the transatlantic value + 20 %.

**Built (N208 net-prediction-high-rtt, ADR-8219).** `ClientPrediction::resim_depth` = RTT ticks + margin target + 2 +
one state send interval, between `max_resim_ticks` and the history (`history_ticks`, clamped to 128). Over the depth,
and when S left the ring, the client writes the server's S into every predicted entity and re-runs to the client tick
from its input history (a reset, not a clear); only a rollback deeper than the 128-tick input history still clears and
snaps. "Snap to the server state" is therefore the simulated state only: the rendered pose smooths through
`NetVisualOffset` unless the offset is over `snap_distance_m` (counted in `PredictionStats::snaps`). The start catches up
past the old cap, and with a transport the clock waits for its first round-trip sample (at most 120 steps) so the first
lead is measured (§ 9.2); the transport's jitter starts at 0, not `RTT/2`. One rollback's re-run is not spread over
frames (the brief's per-frame budget): the re-run is bounded by the input history and runs at most once per frame.
Evidence: `crates/regia_net_soak/tests/time_harness.rs` `high_rtt_prediction_stays_on_for_five_minutes` and
`startup_lead_on_transatlantic_is_right_within_two_seconds`.

### 9.5 `RollbackLog` for a game-owned step (DO27 need "net-external-predicted-step") [PUB]

```rust
/// Bounded ring of game-owned snapshots and inputs for prediction and rollback without the ECS (spec 120 § 9.5).
pub struct RollbackLog<S: Clone, I: Clone> { /* private */ }
impl<S: Clone, I: Clone> RollbackLog<S, I> {
    /// A log that keeps `capacity_ticks` ticks (memory = capacity x (size_of S + inputs)).
    pub fn new(capacity_ticks: u32) -> Self;
    /// Records the state after stepping `tick` and the inputs used for it.
    pub fn record(&mut self, tick: u64, state: S, inputs: TickInputs<I>);
    /// Replaces the inputs of a past tick (a late real input arrived); the next reconcile replays from there.
    pub fn amend_inputs(&mut self, tick: u64, inputs: TickInputs<I>) -> Result<(), RollbackError>;
    /// Restores `server_state` at `server_tick` and replays the stored inputs to the newest tick through `step`.
    /// Returns the corrected newest state; `rollback_too_old` when `server_tick` is older than the ring.
    pub fn reconcile(&mut self, server_tick: u64, server_state: S,
                     step: impl FnMut(&mut S, u64, &TickInputs<I>)) -> Result<&S, RollbackError>;
}
/// A presentation offset that decays to zero over a fixed number of ticks (no wall clock).
pub struct VisualOffset { /* private */ }
```

**Built (lane sprint-net-rollback-log, ADR-7953).** `RollbackLog::new`, `record`, `amend_inputs` and `reconcile` match the signatures above. `reconcile` calls `step` once per tick in `(server_tick, newest]`, which is `newest - server_tick` calls when that range is stored without a gap, and returns `rollback_too_old` without rewriting snapshots or inputs when `server_tick` is older than the oldest retained tick (the too-old counter still increments). A tick that is not older than the ring but was never stored, and a gap between the corrected tick and the newest tick, return `rollback_not_recorded` and also write nothing. The ring keeps at most `capacity_ticks` snapshots; `0` keeps none. `VisualOffset` had no methods in the signature block: `new`, `from_correction`, `value` (`start * remaining / span`, truncating), `advance`, `apply`, `span_ticks` and `is_zero`. The value is the full delta at construction and `0` after `span_ticks` advances; its magnitude never increases; it does not read a clock. A reconcile that re-simulates at least one tick counts as one replay; `max_depth` is the largest `newest - server_tick` of a successful reconcile. The host publishes those counters and `net.rollback` reports `replays`, `depth` (that maximum) and `too_old`. The module does not use the ECS.

Used by the ECS-free predictor, by rollback lockstep (§ 7.7) and by DO27's own step. The agent surface reports replays, maximum
depth and too-old counts (§ 13.3).

### 9.6 Lag compensation [PUB]

The rewind window is per shooter: `RTT + lead + interpolation delay + 1 tick`, measured by the server for that shooter, capped
by `max_rewind_ms` (default **400 ms**, project-settable up to 1,000 ms; owner question Q5). A claimed view tick older than the
shooter's own measured age is clamped to it (today any claim within the window is believed, `lagcomp.rs:2540`). The default 250
ms clamps from about 130-180 ms RTT today.

**Built (N210 net-lagcomp-window, ADR-8315).** `regia_net::lagcomp::window`: the server checks each consumed
command's view tick (`set_view_tick`) against the shooter's window `min(RTT + lead + interpolation delay + 1 tick +
allowance, max_rewind_ms)`, the allowance being 50 ms + 2 × the RTT variation; RTT and variation come from the server
endpoint's peer report (or `set_shooter_rtt`), the lead is the averaged arrival margin, and the interpolation delay is
the reported one bounded by the server's model of the client's clock (target delay at the server's send rate and 4 ×
the variation, falling no faster than `lower_rate`, starting at `max_delay_ms`). A tick older than the window, newer
than the present, or more than the allowance behind the previous view is clamped and counted per shooter
(`lagcomp.stats` `windows`); hit claims compare with the clamped tick. A peer the server never measured gets
`max_rewind_ms`. **Refinement:** the default cap is **1,000 ms**, not 400: § 1.1 promises exact lag compensation to
650 ms RTT and § 16 P5 to 450 ms "with defaults", and a 450 ms shooter's honest view is about 550-600 ms old (RTT +
lead + interpolation), so a 400 ms cap clamps it. The per-shooter window keeps short links short whatever the cap;
the cap only decides how far long links reach (Q5 stays open: a game that prefers 400 sets it). `max_rewind_ms` is
clamped to 1,000. Tests: `regia_net/tests/lagcomp_window.rs`, `regia_net_soak/tests/lagcomp_window.rs`.

### 9.7 Interpolation [PUB]

The jitter buffer delay adds a loss term (the expected gap from the loss estimate) to `2 send intervals + 2 x jitter`; entities
without `NetVelocity` that the budget defers are extrapolated from their last two states for up to one deferral interval instead
of stopping (`interp.rs:677-683`). Bars: a budget-deferred constant-speed entity never drops under 80 % of its speed;
extrapolation on <= 1 % of frames on bad-wifi.

**Built (N206 net-time-harness, ADR-8185): the time harness.** The § 9 bars are measured on
`regia_net_soak::timed::TimedHub`: each process is an ECS world (the lockstep hub's tick) behind a `NetLink` on the
impairment network, so reliable frames go through the transport's resends and prediction reads the transport's RTT
estimator. Time is virtual microseconds; every process updates once per tick of its own clock (`LinkSpec::clock_ppm`),
clients at a seeded phase inside the server's tick, datagrams at their microsecond; RTT steps are link phases
(`rtt-step`) and `timed::rtt_ramp` builds ramps. `TimeReport` gives per client, from 2 s after the connect:
corrections, resets, resyncs, dilation, backward steps, missing and late inputs, the input lead error at the server
(arrival margin minus the client's margin target, per update with a newer input), the RTT estimate's error and its
settle time after each path change, resends and disconnects. The harness sets no bars; the 10-minute rtt-step and
clock-drift sessions (`crates/regia_net_soak/tests/time_harness.rs`) print the numbers the clock, lead and
prediction lanes set them on.

**Built (N207 net-clock-filters, ADR-8312): the interpolation clock (§ 9.1's 8-sample rule, row R6).** RFC 5905's
popcorn spike suppressor in front of spec 74's estimate: an arrival later than the estimate by more than 3 x the larger
of 2 x jitter and one send interval is held out unless a second one follows within 8 arrivals. The estimate and the
jitter stay spec 74's mean and mean absolute deviation of the last 64 arrivals taken; a rise of the offset is slewed in
at 5 % of local time (the estimate never steps backward), a fall is taken at once. One state packet delayed 300 ms
moves neither the estimate nor the target delay (it moved both for 64 arrivals); two late arrivals within 8 are
jitter, as before. Not built as the N207 brief worded it ("a low percentile, not the mean"): a 10th-percentile anchor
with an interquartile jitter met the spike bar but failed spec 74's `delay_adapts_up_fast_down_slowly_and_stalls_resync_once`
(the delay rose 1.1 ms within 0.5 s of a jitter jump: a quartile needs a quarter of the window to move) and
`jitter_settles_and_never_starves` (15 ms off target, bar 12), and moved `net_acceptance` n03's view ticks. The
prediction path's predicted tick can still step back on a lead resync with a negative jump (dilation, § 9.3, N209).

**Built (N211, ADR-8418):** the loss term is how much the longest gap between received state frames exceeds the
`delay_send_intervals` send intervals the delay already holds, over the last 10 s (`LOSS_MEMORY_MS`; a reordered frame splits its gap, a gap over `max_delay_ms` is an outage and is
not counted), still clamped to `max_delay_ms`. A proxy's own update interval (spacing of its last two states, at most
`MAX_DEFERRAL_MS` = 500 ms) sets how long it may extrapolate before its next state is overdue; a rest's gap-rule
blend stays on the send interval (the next state arrives only about one delay before its tick). Refinement: the inferred extrapolation runs until
the proxy's next state is overdue against the newest received frame (it stopped), for at most max(own interval,
`max_extrapolate_ms`) rather than one deferral interval, so a non-deferred velocity-less proxy also bridges a loss
burst; it applies only while frames newer than the proxy's newest state arrive without it (deferred); when the
whole stream is late the proxy is held as before (extrapolating a velocity-less target there broke N03's
hit-where-seen at a strafe turn). Agent: `net.interpolation.loss_gap_ms`, `net.proxy_timeline.update_interval_ticks`.

## 10. Session

### 10.1 Timeout policy [D27]+[PUB]

One `TimeoutPolicy` reaches every layer (today `session.timeout_s` never reaches the transport configs,
`net_session.rs:891-897`, :957-963, and `NetLink` has none, `link.rs:569-577`): `interrupt_ms` (default 1,500), `timeout_ms`
(default **20,000**, mobile-friendly; owner question Q4), `start_grace_ms` (default 30,000 for loaders), all project and server
settable. Removal is for silence only, never ping or loss [D27 A5]. Keepalives are paced by the rate controller's floor. DO27's
stall edges are tests: 3 s and 8 s keep the seat, a silent seat is removed at `timeout_ms` +- 1 tick, a slow loader inside the
start grace is kept, a 3,000 ms RTT 20 % loss peer is never removed.

Built (N101, ADR-7950): `TimeoutPolicy` now reaches both `NetSession` and `NetLink` endpoints;
`config/net.ron` v3 and `server.toml` v4 have migrators. Legacy `timeout_s` remains a compatibility
override; millisecond settings are canonical on write. Startup grace is an admission floor until
game traffic (`InGame` for the session adapter), independent of loading/authentication deadlines.
The 12 s stall acceptance explicitly selects a 10 s timeout; defaults remain 20 s.

### 10.2 The `Interrupted` state [NEW]

Both ends move a peer to `Interrupted` after `interrupt_ms` without an authentic datagram (or when the circuit breaker of § 4.2
trips), raise `PeerInterrupted` / `PeerRestored` events once per outage, freeze that peer's pawn on other clients with a
`NetInterrupted` marker component (replication) or `Absent` seat (lockstep), and keep everything else running. The game shows
"reconnecting" from the event. The client keeps sending keepalives at the floor rate, then probes with exponential backoff and
jitter (250 ms doubling to 4 s).

Built (N101, ADR-7950): interruption is an orthogonal flag, preserving lifecycle phase and peer
identity. Both transports, link/session events, status/peer diagnostics and `session.state` expose
it. The replicated `NetInterrupted` marker freezes proxy interpolation; game movement filters it
on the authority. Empty interruption probes back off from 250 ms to 4 s with deterministic jitter.
Lockstep `Absent` seats remain with N225, which introduces that mode.
Built (N217, ADR-8352): the client-side experience: `LinkEvent::Reconnecting` / `ReconnectFailed` and
`ClientSessionEvent::Reconnecting` / `ReconnectFailed` around the existing Interrupted/Restored pair (one each per outage, also
across an automatic reconnect); the template's `LinkBanner` shows "reconnecting" from them.

Built (ADR-7995): only the client backs off to probes; the server keeps its keepalive floor,
so a client that still hears it sees no gap. Either end restores a peer only when a packet
from it newly acknowledges something (the path works both ways), and a client that hears the
server while interrupted answers at once: a one-way outage raises one event pair per end.
The `NetInterrupted` marker follows the set of interrupted peers (cleared on leave, kick,
retire and re-ownership) and its replication is opt-in, which keeps the v0.5.0 protocol hash.

### 10.3 Resume for every game kind [PUB]+[REG]

- **Token**: the session's rotating 16-byte reconnect token (`lifecycle.rs:1342-1365`) is the resume credential for every game
  kind; `NetLink` and the lockstep layer carry it in the Noise handshake payload (shape after RFC 8446 § 2.2: an opaque ticket the
  server minted, single use, rotated on every welcome). The server binds a resumed transport connection to the existing `PeerId`.
- **Live-token takeover** (adversarial row 7): a valid token presented while the old peer is still `InGame`, `Loading`,
  `Joining` or `Interrupted` replaces the old transport link at once: same `PeerId`, no new allocation, the old link closed
  silently.
- **Slot reclaim** (F6): a returning identity never gets `full` while its own old slot is held; its token reclaims it.
- **Same-address rejoin** (adversarial row 8): a new handshake from the address of a quiet sealed peer enters grace (`Timeout`),
  never `ClientQuit`.
- **Authentic Join refreshes liveness** (adversarial row 9).
- **Replication games** (S2): the replication peer is kept dormant through the grace window (entities, ownership, interest state),
  baselines dropped, the first frame after resume is full state; other clients see the pawn frozen with `NetInterrupted`, and a
  game hook decides to despawn or keep at grace end.
- **Automatic reconnect** after a timeout: the client retries with exponential backoff and jitter (1 s doubling to 8 s) for the
  whole grace window, over the same transport kind it connected with (UDP, Steam, QUIC; S7), then gives up with a final event.
- **Backpressure is flow control**, never a kick (`net_session.rs:379-392` today kicks on any reliable send refusal).

Built (N102, ADR-8180, session layer): live-token takeover from `Loading`, `Joining`, `InGame` and interrupted (old link dropped
without a goodbye, `TokenRefused { link }` for a token matching no slot); slot reclaim follows from it on the session's capacity check.
The transport's same-address replacement of a quiet sealed peer reports `Timeout`; every authentic record, the `Join` resend and the
plain same-nonce accept path included, touches the peer. Backpressure and the reliable rate cap hold messages (cap
`MAX_DEFERRED_RELIABLE_BYTES`, 8 MiB per link); streams pause while the window is half full or the peer is interrupted; an overflow on an
interrupted link enters grace. `NetSession::resume` uses the `SocketOpener` the session connected with (`connect_via`; the lobby passes
its transport). The session core follows authenticated rebinds. Not built here: the token in the `NetLink` / lockstep Noise payload,
replication-game dormancy, automatic reconnect with backoff; the transport-only soak (`NetServer` without a session, F6's harness) has no
token, so its F6 number waits for the transport resume ticket.

Built (N216, ADR-8298, replication games): the transport's join token carries an optional 16-byte resume ticket and its `Accept`
mints a fresh one for every connection (keyed BLAKE3 of the server secret, peer and a counter; single use; wire 5, the v0.6 line's
bump). With `ServerConfig::resume_grace_ms` above 0 (`NetLink` sets 60 s; the session's own transport keeps 0 and its own token) a
player that times out is `PeerDormant` instead of `PeerLeft`: its id is reserved in the transport session (slot counted, never
reissued) and the runtime keeps its `NetNode` link, entities, ownership and interest; its replication connection state is dropped,
unreliable messages are dropped and reliable ones are held with their queue tick (bounded; oldest dropped, never a kick). A join
with the ticket is `PeerResumed` with the same `PeerId` (a takeover when the old connection is still held, before the address and
capacity checks); held messages older than the grace go, and a fresh connection state makes the first gather a join gather (full
state). Grace end runs `NetGraceHook::on_grace_expired` once (`Despawn` or `Keep`; no hook: nothing changes, the game sees `Left`).
Not built: automatic client reconnect with backoff (the game calls `NetLink::resume`), entities despawned while a peer was dormant
are not despawned on its client by the resume (no despawn record reaches a connection that held no copy), the host does not
apply `session.reconnect_grace_s` to `HostedLink` (it keeps the game's `LinkOptions` value), and lockstep's ticket (N225).

Built (N217, ADR-8352, automatic reconnect): `regia_net::reconnect` is the shared timeline (attempt n after `min(1 s * 2^(n-1),
8 s)` minus at most an eighth of jitter seeded from the ticket; one give-up at the window's end). An attempt is a whole handshake
that keeps resending at the handshake cadence until the next attempt replaces it, so the backoff spaces fresh attempts (nonce,
socket, NAT mapping), not packets. `NetLink` (window 60 s) and `NetSession` (window `reconnect_grace_s`) swallow the timeout's
end, emit `Reconnecting { attempt }`, end the outage with `PeerRestored` or with `ReconnectFailed` then the end event; sockets come
from the connect's opener (`connect_via`, `connect_udp`, the session's `SocketOpener`) or the client's own socket (`connect`,
`connect_on`), so Steam and QUIC clients reconnect on Steam and QUIC. Refinement: with an opener, a socket that fails
`DEAD_SOCKET_SEND_ERRORS` times while interrupted starts reconnecting at once instead of at the timeout (wifi-to-cell). `net.status`
shows `reconnect` and `state: "reconnecting"`.

Built (ADR-8535, lost accept): a resumed peer is in the transport session's address index (`add_peer_as`), so a handshake
resend after a lost accept gets the same accept again instead of a second admit with a spent ticket. A ticket (or the session's reconnect token) is spent when the client proves it holds the newer
one, not when the server sends it. A resumed connection keeps the ticket it resumed with until the client's first
authenticated packet on it (session layer: until a `LevelReady`, `InGame` or `Game` from it); until then a join presenting
that ticket, from any address and nonce, is the same resume again (`resume_retries`), also after the unconfirmed connection
timed out into dormancy. So an attempt whose accept or welcome was lost before the next attempt replaced it no longer joins
as a new peer. After confirmation the old ticket matches nothing from anywhere (`resumes_refused`, a fresh join).

Built (ADR-8623, ticket chain): a peer keeps every older ticket (and session token) the client may still hold, not only the
one its last resume presented: two resumes before a confirmation no longer drop the first ticket. At most 8 per peer, each
good for one grace window (session: `max(reconnect_grace_s, loading_timeout_s)`) after it was last superseded or presented;
the client's confirmation spends them all. A session peer a resume put back in `Loading` or `Joining` keeps its grace slot
on a lost link, like an in-game peer. A live takeover forgets the old connection's session token. A copy of a join
answer that already made a peer, from the same address after that peer left it, is refused (no ghost second peer).

Built (ADR-8733, per-address holds; amends ADR-3283): with `resume_grace_ms` above 0, a player that leaves holding its
ticket (a goodbye, or a timeout into dormancy) keeps its slot of `max_peers_per_ip` for the grace. Its return with the
ticket from the same address block is never counted as a new peer of the block (a goodbye's return is a new `PeerId`;
`held_slot_returns`); a live takeover at a full address is not refused either. The hold is claimed by that join and spent
only when its client confirms the accept (ADR-8883): a retry with the same ticket after a lost accept is the same peer,
also after it timed out into dormancy, and a join from another block neither uses nor spends the hold. A fresh join counts the block's live peers
and its held slots and may borrow at most one held slot, so a block holds at most `max_peers_per_ip` + 1 peers through
joins however its players come and go (the nine-behind-one-CGNAT row of § 1 with the default cap of 8). Not built: the
one-round-trip ticket resume (ADR-8733 (3), a v0.7 wire 6 design: the resume credential, the identity proof and the key
share in the first flight).

Built (wire 6 part 1, ADR-8858, which amends ADR-8733 (3)): the credential of a one-round-trip resume and the client's half
of it. A sealed `Accept` carries a 142-byte **resumption ticket** when `resume_grace_ms` is above 0 (a plain link has no PSK
and gets none): readable only by the server that sealed it, it names the peer, the peer's epoch, the client's address
block, one grace of life, the protocol id and build, and holds the link's resumption PSK (derived by both ends from the
handshake, label "regia resume psk", never sent). The client keeps ticket and PSK as a `ResumeCredential`
(`NetClient::resumption`, `ClientConfig::resumption`) and opens its next connect with `ResumeInit` (packet kind 11, padded
to 1,200 bytes): the ticket and message 1 of `Noise_NNpsk0`, whose payload is the join's name, role, build and join data
and a flight number, and nothing else (no 0-RTT data). `ResumeReply` (kind 12) is message 2 followed by the `Accept` as a
sealed record under the new link; the client is connected when it opens, and its next flush is the sealed record that
confirms its keys. `ResumeRetry` (kind 13) asks a source outside the ticket's block to send the same flight again with a
cookie. **Amended design (not all built):** the server admits nothing on `ResumeInit`; it admits when the first sealed
record under the new keys arrives (key confirmation). A ticket is single use through the peer's two epochs (`ResumeEpochs`:
a ticket is good while `confirmed <= epoch <= minted`; the client's first authenticated packet on a connection confirms its
accept and spends every older ticket), which the server already keeps. Flights are ordered by the client's own flight
number, never by arrival. Not built (lane v07-wire6-resume-server): the server's admission of a `ResumeInit` and its replay
register. This build's server reads a `ResumeInit` and refuses it to the full handshake with `resume_unavailable` (a
`Challenge` for the same nonce), so a client with a credential joins in the same three round trips as before, through the
16-byte ticket in its join token; `NetLink`, the reconnect timeline and the soak do not present the credential yet.

Built (wire 6 part 2, ADR-8902): the server admits the resume, at key confirmation. A `ResumeInit` that passes every check
(§ 11.4) admits nobody: it leaves a pending resume (the finished link, the stored `ResumeReply`, the accept's new tickets)
and is answered. The peer is admitted when the first sealed record under the new keys arrives, by the rules every join
passes: a dormant peer or a live takeover keeps its `PeerId` (`PeerResumed`); the held slot of a player that left on
purpose, presented from that slot's own block, is a new peer under the id its accept already named (`PeerJoined`,
`held_slot_returns`), and the hold is spent by that admission, never earlier. Until that record a replayed flight, from
the ticket's block or any other, changes no peer, address, hold or ticket; an admission that fails at the confirmation
(full, per-address cap, banned, a ticket another resume spent meanwhile) is a sealed `Disconnect` on the pending link and
changes nothing else. A ticket is single use: the confirmation moves the peer's `confirmed` epoch past it, and its
register row leaves. One round trip from inside the ticket's block, two from another block (`ResumeRetry`, a cookie). The
confirming record must come from the address the flight proved or from the ticket's block. `net.status`:
`totals.resumed_one_rtt`, `resume_replays`, `resume_lapsed`, and `resume_refusals` by lowercase code. Measured on the
impaired in-process network over `asym-bufferbloat` (`regia_net_soak` test `resume_one_rtt`, four seeds): one flight (the
client sends it again while its answer waits in the queue; each copy gets the stored reply) and one pass of the 1 s queue,
inside the 3,000 ms reconnect grace; the same rejoin with the 16-byte ticket alone takes three passes and misses it
(3,140 ms at the gate seed). Not built (part 3): `NetLink`, the reconnect timeline, `NetSession` and the soak's scripted rejoin present
the credential.

Built (wire 6 part 3, ADR-10088): every reconnect path presents the credential. `NetLink` keeps the resumption
credential of its last accept beside the ticket (a new accept replaces both) and presents both on `resume`, `resume_on`
and the automatic reconnect, on its own socket or on fresh sockets of the link's transport (dedicated, listen, relay and
Steam links share the path); each attempt of one outage is a newer flight of the same credential. `NetSession` does the
same on every `resume: true` connect (automatic reconnect, `resume`, `resume_on`, the game's own connect, direct or by
join code), and its host's transport keeps tickets for `reconnect_grace_s`: a timed-out player is dormant at the
transport under its link id while the session holds it in grace, the link comes back in one round trip, and the
`Hello` with the session's reconnect token resumes the player on it. A transport takeover in place (the client returns
before the host's timeout) is the session's live-token takeover deferred to that link's hello: the peer is detached and
keeps its state, its token resumes it with no drop, and any other outcome of the link is the loss of the old link. A
link the session closes while it holds the player in grace (a suspend's goodbye, its own silence timeout, an overflow on
a silent link) keeps its transport ticket (`NetServer::kick_held`); a timed-out player the session does not hold loses
the transport's hold at once (`NetServer::forget_dormant`). The three-round-trip handshake with the 16-byte ticket
remains as the fallback: a plain link, a grace of 0, a refused resume. The soak's scripted rejoin presents the
credential, and `asym-bufferbloat` keeps tickets as `cgnat` does. Repeat repairs: a reliable fragment lost twice or more
in a row is repaired a deterministic share of a round trip later than one round trip, so a repair does not meet a full
drop-tail queue at the instant that dropped the last one and the repairs of one burst leave apart (the
`reassembly_stalled` ends the credential alone exposed on `asym-bufferbloat`, 3 of 12 rows; § 4). A residual remains,
1 of 72 rows: that queue admits a full-size packet 48 % of the time and the client's own snapshots take the room, so a
fragment can still miss every try inside the stall deadline (ADR-10088 has the measurement and the two fixes left; ADR-10125 found why
the repair lost, its place in its poll, and built the sender's part of the fix; ADR-10189 cuts the fragment to half a
packet, which that queue admits: § 5.3, Built notes). Measured (`docs/perf/net-resume-wire6.md`): the gate plan's two `asym-bufferbloat`
`reconnect_within_grace` rows connect 1,040 ms after the link is up (3,104 to 3,376 ms before), inside the 3,000 ms
grace, and the plan has no red gating bar on two seeds. Not built: lockstep's resume (v07-net-wire6-window-records).

Built (ADR-10093, the soak's outage rows): the gate plan's `flappy` and `long-outage` rows now run this section. Their
servers keep tickets for 60 s, and the soak client drives the reconnect timeline with its resume ticket after a timeout,
as `NetLink` does. Until then both rows joined afresh after every outage beyond the removal timeout, against a server
that kept no tickets, so `interrupted_once` and `resume_same_peer` failed there whatever the engine did. Both bars are
green on every gate row of their profiles and gate the soak from that commit (§ 15.2). Numbers and the in-process tests are
in `book/agents/net-soak.md` and the lane's release fragment.

Built (wire 6, ADR-10052 (2), lockstep's ticket): the lockstep layer carries no token of its own on the wire; its
credential is the transport's resume ticket in the sealed handshake payload. `lockstep::net::SeatPeers` binds seats to
peers and follows `NetServer`'s events: a seat's peer that goes dormant keeps the seat (`SeatLink::Dormant`), and its
return with the ticket (`PeerResumed`, same `PeerId`) resumes the seat in the same server pass through
`LockstepHost::resume_seat` (the token's rule: a member, or removed for silence within `resume_window_ms`), so the
seat's first payload leaves right behind the accept, a round trip before the token flow's (fresh join, then the u64
`resume_token` as a game message; kept for a restarted seat process). `tests/lockstep_seat_resume.rs` measures both on
a 120 ms round trip. Not built: a soak row (the lockstep soak driver has no scripted reconnect).

### 10.4 Address migration with path validation [PUB]

Baseline: a sealed peer moves to a new address on the first authentic newer record (`server.rs:924-989`), so an on-path
attacker that forwards a fresh captured datagram from its own address takes the peer's downstream (F1: 28 hijacks in a soak run;
adversarial row 11); eight forged records a second from anywhere pin a peer whose NAT really moved (row 12); plain links and the
client side do not migrate at all (row 13); a CGNAT address block holding 8 peers refuses a ninth (S3).

Design (RFC 9000 § 8.2, § 9.3): an authentic record from a new address makes it a **candidate**; the server keeps sending to the
old address and sends one packet to the candidate whose packet number is sent only there (a path probe; transport v2 adds a
PATH_CHALLENGE-style 8-byte nonce frame, and the v0.5.x form uses an ack of a candidate-only packet number, so it needs no wire
change); the peer moves only when a record from the candidate acknowledges it. Until then, records from the candidate are
accepted (data is authentic) but the replay window commits normally and the old path stays primary. Attempt budgets are **per
candidate address**, not per peer, so forgeries from elsewhere cannot use up the real new path's tries. The client accepts a
server-side address change the same way (a sealed record proving the session from a new server address, then a probe). Address
blocks count peers per block for admission limits but a rebind into a full block is allowed (the peer already holds a slot). A
completed migration resets the rate controller to its startup state and the RTT estimate to the new path's first sample (RFC 9000
§ 9.4).

Built (N103, ADR-8299, v0.5.x form): as designed, with these refinements. The probe is an acknowledgment-only packet with the
next sequence number, sent only to the candidate; it never counts as lost on the validated path. An unanswered probe is resent
at most every 100 ms; at most four candidates per link are tracked (idle ones forgotten after 5 s). A genuine *duplicate* of one
of the peer's last 64 records from a new address also makes it a candidate (nothing in it is delivered twice): without it, a
real NAT move while an observer races every record would leave the peer on its dead old address, because every genuine record
would arrive as a replay. A stale but never-seen record from a non-primary address (reordered across a move) is delivered on
the validated path without becoming a candidate. The per-candidate budget is a 4,096-slot table of per-second counters keyed
by a secret hash of (token, address), so its memory is fixed and a forged source cannot pick the genuine address's slot. The
one-second spacing between moves is gone (`REBIND_MIN_INTERVAL_MS` kept, unused). No rate controller exists on the v0.5.x
connection yet, so the § 9.4 reset is the RTT estimate and the loss window only; outage-recovery pacing carries on. The
per-datagram address lookup and the token lookup are hash maps (`Session::addr_probes` counts one probe per datagram).

Built (integ-v06p-net, ADR-8387): on transport v2 (§ 4.1, ADR-8184) the probe is a ping with the next packet number, because
v2 never acknowledges an acknowledgment-only packet and acknowledges a lone ping at once; a record from the candidate whose
acknowledgment frame names that number in one of its ranges validates the path. The probe is created lost and flagged: its
acknowledgment is no round-trip, loss, reordering or path MTU sample and moves neither the largest acknowledged number nor the
round or recovery budget. A plain link's "newer record" check decodes the truncated packet number against the largest
received (`Connection::is_newer_packet`).
`net.peers` rows gained `rebinds`, `rebinds_refused`, `path_probes`, `path_candidate`; totals gained `path_probes`.

Built (net-lockstep-nat-rebind, ADR-8422): a NAT rebinding costs about one round trip and the far end keeps receiving
meanwhile. (1) Every probe to a candidate stays answerable (up to 8): on a path whose round trip is longer than the 100 ms
resend, the first answer arrived after the resend had replaced it, and the move waited. (2) An address that authenticated a
record within 5 s is opened without the per-address budget (a lockstep seat sends about 60 records a second; the budget of 8
refused the records that carried the answer until the next second). (3) While a candidate's last authentic record is newer
than the validated path's (the old path went silent), each data packet is also copied to it, within the 3x limit shared with
the probes (RFC 9000 § 9.3); a copied packet's number also went to the validated path, so a copy never validates anything.
(4) A port-only move keeps the RTT estimate, jitter and loss window (RFC 9000 § 9.4 allows it: port-only changes are commonly
NAT rebindings); a new IP restarts them as above. Both endpoints. Totals and rows gained `path_mirrored`. Measured
(`regia_net_soak` lockstep, `mobile-4g` seats): missed inputs per rebinding back to the profile's loss alone (8 seats seat 2:
69 to 22 of 2,375 ticks; 40 seats seat 30: 80 to 15 of 3,578).

Built (v07-net-ki-transport, ADR-10077, KI-NT-018): on a plain link a packet from a candidate address is held, not
applied, until a packet from there answers the probe (at most 16 packets and 24 KiB per candidate); then the held packets
are applied in arrival order from the validated address. Both endpoints. A forger who copied the cleartext token can no
longer inject anything from an address of its choosing; a spoofer of the validated address still can (a plain link has no
key). A full relay that carries both directions validates like a NAT (KI-NT-019, decided: RFC 9000 § 21.1.3.3).

### 10.5 Host migration (listen servers and lockstep) [D27]+[NEW]

Owner question Q1. Dedicated servers do not migrate (their failure is a crash; resume on restart is out of scope).

**Lockstep re-host** [D27] (cheap: every seat holds the world): each seat keeps a frame cache of 600 frames (10 s at 60 Hz).
When the host goes `Interrupted` past `migrate_after_ms` (default 3,000), every seat ranks the survivors the same way: **best
connected** (the lowest mean RTT to the others, from an RTT matrix the host shares in frames every few seconds; DO27's own spec
asks for it), ties by lowest seat id. The winner listens (the agreed port or the next of 16), sends `Elect`; seats reconnect,
send the frames the winner lacks, then `Hello`; the winner waits for every `Hello` or 15 s and resumes at the **highest** next
tick so nobody steps back. If the winner dies too, seats move to the next ranked after 3 s. Readout events for the game:
`HostLost`, `Electing`, `Resuming { in_ticks }` (DO27's "HOST LOST -> CONNECTION ESTABLISHED -> GAME RESUMING IN 5..1").

**Authenticated election** [NEW] (DO27's re-host links are unauthenticated): at join the host distributes each seat's Noise
static public key and a per-match migration secret to every seat over the sealed link; `Elect` and the re-host handshake are
Noise sessions pinned to the winner's static key, so only a seat of this match can become host and only seats of this match can
join it.

**Replication games** [D27 successor-journal design]: the host streams to one or two warm successors (the best connected
clients) a full state every 5 s and the input and event log every tick on the bulk stream; on host loss the first successor
becomes the server from the journal (the newest full state plus the log), clients resume to it with their tokens (§ 10.3) and
keep their `PeerId`s and `NetId`s; the second successor takes over if the first does not answer within 3 s. Bar: a 16-player
listen match continues on the successor within 5 s, same `PeerId`s, every entity present.

Built (N301, ADR-8809, lockstep re-host): `regia_net::lockstep::HostMigration`, sans-IO, one per seat; the winner hosts
through `LockstepHost` as a relay host with its own seat on an in-process link. Refinements:
- The frame cache is the seat's last `frame_log_ticks` frame bodies (600) with their delta base.
- The roster (static keys, addresses, agreed port, migration secret, seat generations) and the round-trip matrix are
  lockstep stream payloads. They are not frame fields, so there is no wire version bump. The matrix also lists the
  seats the host found silent: they rank last and the winner does not wait for them.
- Seats that hold matrices of different versions converge on the newer one. Claims and election answers carry it.
- The re-host messages (claim, election answer, hello frames, hello, resume) carry a keyed BLAKE3 MAC under the
  migration secret, bound to the match id, the epoch and the sender's seat. The pinned Noise key covers the winner's
  side. Seats are authenticated by the MAC because the transport's handshake has no client static key.
- Resume tokens are keyed by the migration secret, so the new host accepts the old host's tokens.
- Host silence counts lockstep payloads plus `LockstepSeat::note_host_heard` (the transport's keepalives).
- The countdown is `resume_lead_ms` (1,000).
- A winner that gets no hello by `hello_wait_ms` does not resume alone.

Built (N302, ADR-8853, warm successors: part 1 of the replication-game journal): `regia_net::migration`, sans-IO and in
the ECS-free core. `JournalHost` on the host, a `JournalSuccessor<W: ShadowWorld>` on every client (idle until appointed),
`JournalHost::pump_server` for a `NetServer`, agent method `net.migration`. The hand-off (N303) is not in it. Refinements:
- The game supplies the full state's bytes, a world digest, and per tick two blobs (inputs, events); the shadow world is
  the game's `ShadowWorld` (`load_full`, `apply`, `digest`). The ECS adapter for `NetLink` games is left to N303.
- "Best connected" for a listen server is the client's own link to the host: round trip + variation + 10 ms per percent
  of loss (smoothed over 10 s), ties by peer id. A client whose send allowance is under 16 KiB/s is starved and ranks after every other;
  interrupted clients are never picked. Successors are sticky: replaced when they leave or go silent, when they fell
  out of the journal and a link that is not starved waits, or when another's score is under half and 20 ms better
  (checked every 10 s).
- The budget is a share (`budget_share`, default a quarter) of `NetServer::send_allowance` for that peer, as a token
  bucket; the transport still schedules the bulk stream last.
- One poll hands a successor one payload of at most 900 bytes (the log and a part of the full state in one stream
  fragment), and the next leaves only when the stream has acknowledged the one before (one may be on its way while the
  allowance is at least 32 KiB/s). Two measurements forced this. On good-wifi, 8 KiB full-state parts stood in front
  of the log inside the ordered bulk stream for up to 17 ticks; small parts keep the true lag at 1 tick p99. On
  bad-wifi, a journal that kept feeding the bulk stream cost 7 of 8 clients their connection within 70 s
  (`reassembly_stalled`): at the rate floor the sender's pacing holds bulk repairs back past the receiver's stall
  deadline (§ 5.2) while later fragments arrive. With one fragment at a time nothing is stored behind a gap.
- The log goes before full-state data, and reaches a full state's tick before that full state, so the shadow compares
  digests by replay first. A shadow that matched adopts the full state without loading it; any other loads it, checks
  again and replays the log it kept.
- A log payload has a five-byte header and a flags byte per tick (an idle tick is six bytes), because at the rate
  controller's floor (4 KiB/s) a quarter share is 1 KiB/s.
- The journal depth is 600 ticks after the newest full state, on both sides; one past is `journal_past_window`. The
  host's log waits at the window for the next full state; a successor whose next record left the host's ring (two
  depths) is seeded again.
- Acknowledgments (applied tick, state, digest counters) come back unreliable and cumulative; the host reads the lag
  from them.
- A full state is wanted on the cadence only while a successor is free to start it, and at most one extra per second
  for a successor the newest cannot seed.

Built (N303, ADR-8901, the hand-off: part 2 of the replication-game journal): `regia_net::handoff`, sans-IO and in the
ECS-free core. `HandoffHost` on the host (`pump_server` next to the journal's), a `HandoffClient` on every client,
`PromotionPlan` for the promoted successor, `NetServer::{adopt_dormant, keep_peer_id, start_at_tick}`, and the
`handoff` object of `net.migration`. Refinements:
- "Clients resume to it with their tokens" needs a token the successor can check before it is a server. The host
  derives one secret per successor (keyed BLAKE3 of its master secret over the successor's id and announced key) and
  gives it to that successor only; a client's ticket for a successor is a keyed BLAKE3 of that secret over the
  client's id. A successor cannot make a ticket for the other successor's server. The ticket is presented as the
  resume ticket of § 10.3; the promoted server holds every player as a dormant peer (`adopt_dormant`).
- Each successor makes the key pair of the server it may become and announces the public half; the roster gives it to
  every client, and a resuming client pins it. Path validation is the join handshake's cookie exchange, unchanged.
- The roster (successors in order, addresses, keys, the client's tickets, the two thresholds) and the grant (the
  secret, the players) are game messages on the reliable channel of the sealed link: no wire version bump. The
  roster's order is the journal's and moves only when the set of successors does (every change of the order is a
  moment in which two clients can hold different rosters).
- The host counts as lost after 1.5 s of silence (`host_lost_ms`, the transport's interruption threshold) or when the
  connection ended. Each successor has `takeover_ms` (3 s) from the moment a client starts on it.
- A client keeps its connection to the host while it tries a successor on a second socket, and stays with the host
  when it hears it again first, so a client's own outage costs it nothing.
- "The first successor becomes the server" is two steps. At its turn the successor **arms**: it opens its server on
  the port it announced, holds the players and counts ticks from the shadow's tick, with its shadow and its
  connection to the host untouched. It **commits** (promotes the shadow, simulates on) when a majority of the players
  has resumed to it or its own connection to the host ended, and **disarms** when it hears the host again first. A
  successor in its own outage therefore never takes anything over. A successor that announced no port has one
  socket and promotes at once; one that took over without a majority reports `NoQuorum` after 10 s.
- A resume attempt resends its handshake packets every 100 ms (the transport's default is 250 ms).
- The lost host's own player (owned by `PeerId::SERVER`) gets the lowest `PeerId` above every player's on the promoted
  server and is held dormant like the others; a host that was only cut off comes back with
  `HandoffHost::comeback`. The promoted successor keeps its own `PeerId` for its player (`keep_peer_id`).
- A successor serves at the address the host saw and the port it announced. Reaching a successor behind address
  translation is a port mapping (§ 10.7) or the relay (§ 10.6), as for a host; neither is wired to the hand-off.
- The match continues from the successor's shadow: ticks the journal had not delivered are rolled back for everybody.
- The `NetLink` (ECS) adapter is still not built: the game writes its `ShadowWorld` and moves the old host's entities
  to the kept seat itself.
- Measured (16 players, the soak harness's links, three seeds each, from the host stopping to the last client being
  accepted by the successor): 1.65 to 1.75 s on good-wifi and 2.14 to 2.32 s on bad-wifi (bar 5 s); first successor
  dead too: 4.65 to 4.75 s and 5.02 to 6.08 s (bar 8 s). Same `PeerId`s, the promoted world equal to the lost host's
  at the promoted tick, every player entity present.

### 10.6 Relay for many clients (DO27 need "net-relay-many-clients") [PUB]

Baseline: one relayed client per host address (`relay.rs:680-705`, `RelayError::Busy`; test `a_flow_in_use_is_not_displaced`);
DO27 needs 16 per player host and 32 (later 40) per community dedicated server, with no paid servers.

Decision: **one relay allocation per client** (the TURN shape, RFC 8656 § 5), not a client-id header. A `Bind` from a client gives
it its own relay-side port; the host sees each relayed client as a distinct source address (`relay_ip:port_k`), so the Noise
handshake, the record layer's per-peer keys, `NetServer`'s peer table and rebinding need no change, and a v0.5.x host can be
relayed to many clients without a wire change on the game path. Rejected: a client-id header on relayed datagrams changes the
framing every layer above sees and would make old hosts unrelayable. Costs: one relay port per client (the relay's default
pool is 4,096 ports, `RELAY_MAX_FLOWS`), one NAT mapping per client on the host side (kept alive by the game's own keepalives).

- **Flow table** keyed by allocation: `(host, client) -> port`; caps: per host `max_clients_per_host` (default 40, operator
  setting), global flows (4,096), per source IP allocations (8).
- **Permissions** (RFC 8656 § 9): an allocation forwards only between the bound client address and the host address; datagrams
  from any other source to the allocation port are dropped (H2: today any source that knows a flow can spend it). A client that
  rebinds refreshes its permission with the bind key, which the relay checks in constant time.
- **Busy and full**: over the per-host cap, `Bind` answers `Full { cap }`; an idle allocation (5 s) may be replaced by the same
  client key; an active one is never displaced (kept).
- **Bandwidth**: per allocation 4 MiB/s per direction (kept), per host `host_bytes_per_second` (default 64 MiB/s), global by the
  operator. Worked cost: a 32-client match at DO27's state-sync budget (230 kbit/s down, about 40 kbit/s up per client) moves
  about 7.4 Mbit/s host-to-clients and 1.3 Mbit/s clients-to-host through the relay; each byte is received once and sent once,
  so about 8.6 Mbit/s in and 8.6 Mbit/s out, about 3.9 GB in and 3.9 GB out per match hour. A small VPS with a 1 Gbit/s port and a multi-terabyte monthly allowance carries
  dozens of such matches; the community runs it for free (owner question Q2).
- **Abuse limits** (kept and extended): control requests padded to their reply, per-source control rate limits, `Register`
  proves its source with a cookie, directory pages one per request.
- **Version**: relay protocol 2 (`RGNR` magic kept, a version byte added to every control datagram); a v1 request to a v2 relay
  gets the v1 answer for v1 operations (single-client `Bind` semantics) and a `VersionTooOld { min: 2 }` for allocation
  operations; a v2 client talking to a v1 relay gets no allocation reply and falls back to the v1 bind with a logged reason, never
  a hang. The relay status is an agent method (`net.relay_status`).

**Built (N112, ADR-7982, 2026-11-16).** Relay protocol 2 is in `relay.rs` and `relay/alloc.rs`. Refinements: the allocation
operation is its own `Allocate` request (a versioned `Bind` keeps the single-flow meaning); a rebinding side refreshes with its own
key (the client's allocation key or the host's `host_key` from `ClientJoined`), not the lobby's bind key, which every client of the
lobby knows; a `Refresh` from a new source proves it with a cookie, as `Allocate` does; a host behind the relay needs
`max_peers_per_ip` at or above its relayed clients (they share the relay's IP), which N113 sets; `net.relay_status` is N113's (this
lane: the `relay` console command and `RelayStatus`). Tests: `regia_net/tests/relay_allocations.rs` (32 sealed clients over real
UDP, the 33rd `Full`, the forger, protocol 1 answers) and `relay::alloc::tests`.

### 10.7 NAT traversal and hosting (S5) [PUB]+[D27]

- **Candidates from the game socket** (ICE-style, RFC 8445): host candidates (local addresses), server-reflexive (the relay's
  `Lookup`/`Register` reply names the address it saw, the STUN role of RFC 8489), mapped (PCP RFC 6887, NAT-PMP RFC 6886, UPnP IGD,
  raced, first success wins [D27 wiring]), and relayed (§ 10.6). Gathering and registration use the **game socket** (today the
  host registers from a separate socket, `server_host/join.rs`), so the mapping punched is the one the game uses.
- **Connectivity checks**: both sides send paced checks over all candidate pairs in priority order (RFC 8445 § 6); the first pair
  that completes the sealed handshake is nominated; the relay pair starts in parallel after 500 ms and wins if nothing else does
  by 1.5 s. Bar: joins succeed on every NAT type pair of the `SimNat` model; relay fallback within 1.5 s.
- `NetSession::connect(JoinCode)` works end to end (today it errors, `net_session.rs:990-993`).
- **Hosting flow** [D27]: port mapping, a port-forward self-check (the relay connects back to the host's mapped port), and a
  host-capacity estimate before hosting a big match (measured uplink from the rate controller's startup against the relay,
  divided by the per-client budget), shown to the player.

**Built (N113, ADR-8183, 2026-11-17).** Registration from the game socket (`server_host::relay_split`: relay control from the
relay's IP starting `RGNR` goes to the registrar, the rest to the endpoint; renewals every 15 s keep the mapping open; the host
confirms each `ClientJoined` with a `Refresh` from the game socket) and the client's join by code (`server_host::JoinPath`;
`NetSession::connect(JoinCode)`): direct attempt, own allocation from 500 ms, relay wins at 1.5 s. Refinements: a protocol 1
relay ends the join with `relay_too_old` rather than falling back to a protocol 1 bound flow (one client per host); relay IPs
skip the per-IP limits (`NetServer::add_relay_ip`); the direct candidate is the host's reflexive address only (the full
candidate checks are net-nat-candidates); `net.relay_status` is the agent view. Tests: `regia_net/tests/relay_host_flow.rs`
(16 clients behind a symmetric-NAT host by code, 5 minutes relayed, no disconnect; `relay_too_old`; `connect(JoinCode)` over
UDP), `server_host_e2e` (`--relay local:0`), `regia_agent/tests/net_relay_status.rs`.

**Built (N228, ADR-8340, 2026-11-17).** Candidates and checks are `regia_net::nat` (`Candidate`, `CheckAgent`, `CheckPacer`,
`CandidateSource`); relay protocol 3 carries them: a protocol 3 `PunchIntro` names the code and the client's candidates, and the
relay answers both sides with `Introduced` (the other side's reflexive address and candidates, a session key; the client also gets
the ticket and bind key), padded so both answers fit in the request; the host's candidates ride its `Register` after the cookie,
where older relays read padding. Checks (`RGNC`) leave from the game socket at one per 50 ms per session (Ta) and 5 ms between any
two of a process; a check from an unannounced source becomes a peer-reflexive candidate with a triggered check. Refinements: the
client runs its one sealed handshake on the best answered pair and nominates the pair it completes on (RFC 8445 regular
nomination) rather than a handshake per pair; the 1.5 s relay bar counts from the introduction; mapped candidates are the
`CandidateSource` net-nat-hosting-flow implements; `SimNat` gained RFC 4787's address-dependent mapping and filtering. Tests:
`regia_net/tests/nat_candidates.rs` (all 100 host × client pairs of no NAT plus nine NAT behaviors join, direct exactly where the
RFC 4787 model punches; symmetric/symmetric relayed within 1.5 s; relay-path 5 min, 0 disconnects; checks paced), `nat::tests`.

**Built (N229, ADR-8370, 2026-11-17).** The hosting flow is `port_map::PortMapWorker` (PCP, NAT-PMP and UPnP IGD raced by
`race_port_mappers`, first grant wins and a later one is deleted; `PortMapLease` renews at half the lifetime and deletes at
shutdown), `server_host::MappedCandidates` (the mapped candidate, announced with the next `Register`), and
`regia_net::host_check` driven by `RelayRegistrar::start_host_check`: the port-forward self-check is relay protocol 3's
`ProbeRequest`/`ProbeSent`/`ProbeHello` (the relay probes the host's own IP from a probe port the host never sent to), the
capacity estimate is `UplinkProbe` against the relay's `Measure`/`MeasureAck` echo divided by the per-client budget;
`net.host_check` is the agent view. Refinements: the rate controller (`cc.rs`, § 4.2) is not on this line, so the
measurement runs its own bounded BBR-shaped startup (256 KiB/s start, 2/ln 2 pacing gain, full pipe after three rounds
under 25 % growth, at most 3 s and 16 MiB); the probe and measurement messages reuse protocol 3 (no protocol 4); with one relay
IP the self-check cannot tell a port forward from an address-only filter's state toward the relay (RFC 5780 § 4.4). Fixed
while there: one PCP nonce per mapping kept for its renewals and delete (RFC 6887 § 11.1), the UPnP permanent-lease fallback
(error 725); the UPnP HTTP read was already bounded. Tests: `regia_net/tests/port_map_race.rs`,
`regia_net/tests/host_check.rs` (reachable, then unreachable once the forward is removed; listen-uplink estimate within 20 %).

**Built (v07-net-portmap-df-per-os, ADR-8893).** `port_map::default_gateway_v4` reads the routing table on every platform
behind `port_map/platform/<os>.rs`: Linux and Android `/proc/net/route`, then an `RTM_GETROUTE` dump over `NETLINK_ROUTE`
(every table: Android keeps each network's default in a table of its own and denies `/proc/net/route` to apps; no root, no
`bind`); Windows IP Helper `GetIpForwardTable` (lowest `dwForwardMetric1`); macOS the `PF_ROUTE` sysctl with `NET_RT_FLAGS`
/ `RTF_GATEWAY` (an unscoped default before an `RTF_IFSCOPE` one). The byte parsers are public and pure
(`parse_netlink_route_dump`, `parse_ipv4_forward_table`, `parse_bsd_route_dump`). A platform that denies the table, or no
IPv4 default route, stays `Unsupported` and the hosting flow reports `port_map_unavailable`. Tests:
`regia_net/tests/gateway_readers.rs` (captured Linux netlink and macOS sysctl bytes, a documented-layout Windows table, and
the live reader against the host's own tool).

**Built (v07-net-ki-hosting-lobby, ADR-10069, 2026-11-20; N-19).** A host withdraws its join code: a `Register` with a
lifetime of 0 (cookie-checked like any register; no shape change) removes the sender's lobby at once and answers
`NotRegistered`; a later `Lookup` answers `UnknownCode`, bound flows stay. `RelayRegistrar::revoke_code` sends it until
confirmed (`code_revoked`); a relay built before this change clamps the 0 to one second, so the code lapses within it. The dedicated host's `--no-port-map` turns the router mapping off for a remote relay
(the host check reports `port_map_disabled`); the same switch as a `server.toml` key is listed for the v0.7 format batch.
Tests: `relay::tests::a_zero_lifetime_register_withdraws_the_code`,
`relay_host_flow::a_host_revokes_its_join_code_and_a_later_join_by_it_fails`,
`server_host::port_map_switch_tests`.

### 10.8 Steam [REG]

`SteamSocket` stays a `DatagramSocket` under every layer; resume reuses Steam (S7); receive batches are truncated to
`MAX_DATAGRAM_BYTES` before copying and oversize counted per sender, and a failed send is surfaced so the connection does not
think the datagram left (adversarial row 15). Steam's relay network is the relay on Steam builds (owner question Q2).

**Built (N219, ADR-8351, net-impaired-steam-mock).** `MockSteamNet` takes the soak harness's seeded impairment per user
(`MockLink`: the harness's direction model, i.e. outages, random and Gilbert-Elliott loss, a drop-tail bottleneck,
latency plus order-keeping jitter, reorder and duplication, and its numbers for `clean`, `good-wifi`, `bad-wifi`,
`mobile-4g`, `transatlantic`, `congested` and `flappy`). Time is virtual (`MockSteamNet::advance_ms`); a clean link
(the default) delivers at once and in order, as before. `regia_steam/tests/net_impaired_steam.rs` runs lobby-launched Steam
sessions on good-wifi (60 s), bad-wifi (5 min, zero drops, leaves or disconnects, every reliable message once and in
order) and flappy (3 s outages every 20 s: the player stays, every interruption ends), and a suspend/resume on bad-wifi
that comes back over Steam with the same `PeerId`. Refinements: the mock's links are per user, so a message crosses two
impaired links (the sender's up, the receiver's down), which is harsher than the harness's clean dedicated-server side;
`flappy`'s 12 s outage on every fourth client and `mobile-4g`'s MTU and NAT rebinding are per-client harness extras that a
single link does not carry (`MockLink::outages` adds one-off outages). The harness's `ImpairedNet` and the mock are two
copies of one model until `regia_net_soak` lands on the line; the lead may then point the mock at the harness's types.
Measured: on the v0.6 base with the impaired mock, the bad-wifi and flappy sessions already held (the session layer was not
the gap; the lossless mock was); the Steam resume on bad-wifi failed there (`Disconnected(Suspended)`) and passes with
net-session-resume-fixes. Bad-wifi 5 min: 16,451 messages over the links, 893 dropped (5.4 %), 315 reordered, 147
duplicated, 3,125 reliable messages all delivered in order.

**Built (N104, ADR-7981).** `P2pMessages::receive_bounded` (default over `receive_into`; the real client overrides it so a long
message is never copied); `SteamSocket` reads through it with `MAX_DATAGRAM_BYTES`, counts `SteamSocketStats::oversize` and
`SteamSocket::oversize_by_sender` (at most 1,024 senders); `send_to` returns an error when Steam refuses the send.

**Built (v07-net-ki-hosting-lobby, ADR-10069, 2026-11-20; KI-NT-048, -050, -051).** `SteamAuthSessions` matches each
`ValidateAuthTicketResponse` to the oldest begin of that Steam id still waiting for its first answer (Steam answers begins
in order): the answer of a session a restart replaced, or of one that ended first, no longer refuses the session after
it. `LobbyBackend::clear_server` (default: delete `regia.server`) is what a rolled-back launch calls; the Steam backend
also writes Steam's no-server values over `SetLobbyGameServer`. `MockDirection::mtu_bytes` sends a larger message as
fragments (lost when one is), `MockLink::rebinds_ms` and `rebind_gap_ms` model a NAT rebinding as a reroute gap, and
`MockLink::profile_for_client` adds the harness's per-client extras (`flappy`'s long outage on every fourth client,
`mobile-4g`'s rebinding). `mobile-4g` carries MTU 1,232.

## 11. Security

### 11.1 Kept [REG]

Noise NX with sealed records and a 4,032-record replay window checked before decrypt; stateless padded handshake with
per-source limits; known-hosts pinning; bans by identity and address block; relay padding.

### 11.2 Hostile input policy [REG]+[NEW]

Every refusal is charged to its sender and named (`ServerEvent::Rejected { peer, kind, error }`, ADR-7892). Fatal at once:
over-cap input or command, reassembly over a cap, reassembly stall, stream over its flow-control window, a server-only kind
from a client, a desync bundle or migration message outside its phase. Scored (one point each, the record's credit applied only
after every message in it decoded, limit 1,024 per window): malformed input, malformed message, unknown lane, spectator input,
malformed voice (inside the voice rate bucket). A reliable decode error on the client is a protocol error, never dropped
silently (it desyncs the stream). Lockstep: a seat's input claiming another seat is overwritten with the sender's seat [D27];
command blobs have a count and size cap at the host; axis ranges are checked by the game codec.

**Built (N105, ADR-8181).** The record credit is gone: no packet earns a point back (empty and fragment-only packets earned it
too). The score is a leaky bucket in time instead, one point per refusal, leaking 4 points a second (`MALFORMED_LEAK_PER_S`), limit
1,024 unchanged: one bad message every few seconds is never removed, bad/good alternation at 100 packets a second goes in about
22 s, one bad message per packet plus a 60 KiB bad reliable message a second (soak F3) in about 11 s.

**Built (net-swallowed-decode, ADR-7984).** Spectator input (both input lanes) is `RejectKind::SpectatorInput`, refused
undecoded and scored; a server-only kind from a client is `ServerOnlyMessage` (fatal, `protocol_error`); malformed voice
is `VoiceMalformed`, after a token of the sender's voice bucket; a client ends its link with `protocol_error` on a
reliable decode error of any lane and sends its goodbye, an unreliable one is dropped and counted. Refinement: the
replication runtime sits above the endpoint, so its refusals (server-only kinds, voice) are named as
`runtime::NetRejection` (`NetNode::take_rejections`, `net.status` `rejections`) rather than as `ServerEvent::Rejected`,
and score their own points against the same 1,024 limit (one back per runtime pass with no charge). `RejectKind` is
`#[non_exhaustive]` from here on.

**Built (net-swallowed-decode-fix, ADR-8438).** A runtime pass gives a point back only when the peer delivered an
accepted message and nothing was charged; a voice packet over its bucket withholds that pass's credit, so garbage at twice
the bucket rate still climbs to the limit. A malformed voice control message (unknown kind, block account over 256 bytes)
is `VoiceMalformed`. Runtime refusal logs share the per-peer 10 lines/s budget (spec 70 § 6.4); the relay keeps at most
`MAX_KEPT_VOICE_REJECTS` untaken entries.

**Built (net-hostile-malformed-drop, ADR-8606; amends ADR-8181 and the "one point each" above).** Only traffic the session
authenticated scores: a sealed record that opened under the peer's keys, or a plain-link packet from the peer's validated
address. A sealed record that fails its open (forged, replayed, damaged) gives back the packet and byte tokens and the slice
place the rate check took (at most one slice of such failures per peer per receive; past it they drop unopened, charged to no
one), and a plain packet from an address the peer has not validated (a rebind candidate) is refused and counted, never scored,
never raised as the peer's `Rejected`, never fatal for a message-level kind. Weights (`RejectKind::malformed_weight`,
`MALFORMED_WEIGHT` 16): an authenticated packet the connection cannot parse, `InputMalformed`, `MessageMalformed`,
`UnknownLane` and `VoiceMalformed` score 16 (no lossy, reordering or migrating link produces one: a damaged datagram fails
its tag, a late or duplicated one is dropped before decode, a reconnect is a new key); `SpectatorInput` keeps 1 (well formed,
refused undecoded, a host game's input loop left on); stale, out-of-window, duplicate and replayed packets, reassembly
refusals and version mismatches were never refusals and stay at 0. Leak (4/s) and limit (1,024) unchanged: the soak's 63
refusals a second go in about 1 s, a drip of 5 a second in about 13.5 s, a peer below one refusal every 4 s never climbs, a
burst of 63 is survived. The replication runtime's voice score (ADR-7984/8438) is unchanged (one point, pass credit).

**Built (ADR-8666, amends the ADR-8606 note above).** A sealed record whose tag verified but whose kind byte names no kind,
and an authenticated `Accept` or `Deny` from a client, score 16 like any other authenticated malformation. Failed opens are
no longer capped per receive: every datagram at a peer's address and token is opened on its own merits (bounded by the
16,384-datagram read cap), so a forger who copied the cleartext token cannot hide the peer's own datagrams behind a front of
forgeries. The replication runtime's voice score now follows the server's rule: `RejectKind::malformed_weight` (16 for
`VoiceMalformed`), leaking 4 points a second of ticks, no pass credit. On a plain link, a packet from an unvalidated address
no longer fails the connection for a violation found before it changes any state, nor removes the peer through the reliable
per-receive cap.

**Built (ADR-10077, KI-NT-024/025).** A `ServerBridge` host gets the replication runtime's refusals as
`ServerEvent::Rejected` from `pump_in`, ahead of the endpoint's events; `net.rejections` counts every refusal per peer
and lowercase code, transport and runtime, peers that left included (256 peers kept). The ADR-8666 unvalidated-delivery
path is gone: an unvalidated plain packet never reaches a connection (§ 10.4).

### 11.3 Floods and fairness [PUB]

A per-peer token bucket on packets and bytes **before** the AEAD open (keyed by source address and session token, default 4x
the peer's controller target, burst 64 packets); over it, datagrams are dropped uncounted against others, the peer is scored,
and a sustained flood (2 s over) drops it. Per receive, peers are served round robin (one peer cannot use the 4,096-datagram
receive budget). Bar: a 100k/s flooder next to 7 players: their inputs and RTT within their control values, the flooder dropped
within 2 s (F4).

**Built (N105, ADR-8181).** `regia_net::inbound`, `ServerConfig::inbound`. Packet and byte buckets are charged after the address
and session-token lookup, before the open; a message bucket per delivered message (over it unreliable messages drop). Derived
defaults until N202: packets 4 x `max_packets_per_poll` x tick rate, bytes = packets x `max_packet_bytes`, messages 4,096 x tick
rate, burst 64 packets; each bucket's depth is the burst plus its rate over the time since the last receive (one tick .. one
second), so a once-a-tick receive admits an honest tick. Round robin is built as an equal slice per receive, `max(256, 4,096 /
peers)` datagrams per peer, with up to 16,384 datagrams read (a drop before the open costs one lookup) and 4,096 of real work for
datagrams no peer owns; no datagram is copied to reorder. The flood timer adds each over-rate receive interval and takes half of a
clean one off (intervals over 1 s ignored); at 2,000 ms the peer gets `Rejected { kind: flood }` and leaves with `Flood`. Soak F4
(190k valid messages a second) goes through the message bucket. Test: `regia_net/tests/flood_fairness.rs`.

Decided (ADR-10077, KI-NT-020): a flood above the 16,384-datagram read cap waits in the kernel queue and delays the
other peers' datagrams behind it to the next receive; the defense is in front of the server (firewall, provider
filtering, Steam's relay network). Revisit with a batched receive if a soak row shows a tick over budget under the cap.

### 11.4 Handshake availability [PUB]

Pre-proof answers are rate limited **per source prefix** (/24 IPv4, /56 IPv6) with a fair share of a server-wide budget (default
4,096 answers/s), instead of one server-wide 512/s cap that a forged flood exhausts for everyone; `KeyInit` without a pending row
gets a fresh challenge when the source's prefix has budget. The pending table counts per prefix. Bar: under a 100k/s forged
flood from 1,000 /24s, a real client joins within 5 s in >= 99 % of 200 trials. Cookies bind to the source IP, not the port
(adversarial row 10: a NAT that changes port after the challenge stalls the join), and a later challenge for the same nonce
replaces a stored one until a key reply exists.

Built (N114, ADR-7946): cookies and password nonces bind the canonical source IP,
nonce and time bucket; unusable-cookie KeyInit/plain join responses get a fresh
non-amplifying challenge within the existing answer budget. Changed challenges
replace an exchange only before a valid KeyReply; identical copies keep the key
share, and replacements do not reset the timeout. A replacement stranded by an
already-pending server share gets a budgeted refresh and a bounded fresh-nonce
restart with the same unproven-step deadline; the server still does no second
curve exchange on that pending cookie. No framing change. This Built
note covers the last sentences of § 11.4; prefix fairness is a separate lane.

Built (N218 part, ADR-8718): prefix fairness. Each source prefix (/24, /56) has a token bucket refilled at an even split of
15/16 of `pre_proof_answers_per_s` (default now 4,096) over the prefixes that asked in this or the last second; a prefix
asking alone has the whole rate, and the server-wide cap stays the hard bound. A request the cap sheds still spends its
prefix's token (otherwise every bucket refills while the cap is closed and the flood reopens each second with a burst).
An answer whose cookie comes back valid (`KeyInit`, `ChallengeResponse`) is given back to its prefix and to the
second's count: a blind forger never earns one, so a prefix of real clients (one office, one CGNAT, the soak's whole
client range) is answered as fast as its clients prove their cookies. At most 65,536 prefixes have a bucket (idle ones
are forgotten after two seconds; past the bound new prefixes share one overflow bucket); the map's hash keys are random
per process. Version probe answers draw on the same shares. The pending table still counts per address block, not per
prefix (not built).

Built (ADR-8764): the cap is a true ceiling on the unproved answers of a second. A refund proves one exact answer once:
the (source address, client nonce) a `Challenge` went to in this or the last second. A replayed cookie, a cookie for
another address of the prefix, or one older than a second proves nothing; only a proof of this second's answer gives
back a slot of this second's count, and a plaintext `Deny` sent after a cookie verifies takes the proved answer's slot or
passes the cap as a fresh answer.

Built (ADR-7995): the client keeps the first key exchange whose KeyInit left when a later,
different challenge replaces it (the cookie bucket turned over between two requests on a slow
path), resends both KeyInits and finishes on the share the server answers; a refresh naming
the first challenge goes back to it. A sealed join from another port of the same IP finds its
half-open row by session token and moves only after its tag verified. A bounded set of
admitted (IP, nonce) pairs refuses a replayed plain join answer from another port. Relay
Register cookies bind the exact address under their own MAC domain.

Built (wire 6 part 1, ADR-8858): the availability rules of the resume flight, on the client and in the codec. A
`ResumeInit` is padded to 1,200 bytes and a shorter one is refused undecoded, so no answer to it (`ResumeReply` at most 283
bytes, `ResumeRetry` 29, a `Challenge`, a deny) is larger than the request. A refused resume is answered with a plain
`Challenge` for the same nonce, drawn from the pre-proof answer budget like any other, and the client carries on with the
full handshake from it at no extra round trip; a source outside the ticket's address block gets `ResumeRetry` and resends the
same flight with the cookie (the cookie is not in the Noise prologue, so it is the same flight); a flight whose first Noise
message does not open under the ticket's PSK gets no answer. Opening a ticket is one keyed hash and one AEAD, opening
message 1 one more AEAD: no curve work and no state before the PSK opened. Ticket keys: two live (current and previous),
rotated every resume grace, derived from the server secret and the period's number, so the server keeps no state per ticket
or per key (`TicketKeys`). After three unanswered flights the client also sends its `ConnectRequest`, so a server that
predates wire 6 (it drops kind 11 unread) still answers with the `protocol_mismatch` deny of § 12.2. Not built (lane
v07-wire6-resume-server): the cookie skip for a source inside the ticket's block, and charging `ee` to the handshake budgets
once per distinct flight.

Built (wire 6 part 2, ADR-8902): the server's side of those rules. The checks run cheapest first: the ticket (one AEAD), the
address (inside the ticket's block, or the cookie of a `ResumeRetry`, which is drawn from the pre-proof answer budget and
refunded when its cookie comes back), the ban list, the byte-identical copy of a pending flight (the stored `ResumeReply`
from the pre-proof answer budget: no further AEAD, no `ee`, no handshake charge), message 1 under the PSK (one AEAD; a
failure gets no answer), the hello, who the ticket names, the flight's number, and then the budgets: `ee` is charged to
`handshakes_per_ip_per_s` and `max_handshakes_per_s` once per distinct flight, before the curve work, and a flight over
either is refused to the full handshake (`resume_busy`). A flight whose number is at or below the highest seen with its
ticket and matches no pending row gets no answer (`resume_stale_flight`).

### 11.5 Memory bounds [PUB]

§ 5.2 caps; shared-port queues copy at most `MAX_DATAGRAM_BYTES` (2,048) per datagram with a byte cap per half and a per-source
drop counter (adversarial row 14); Steam receive batches bounded (§ 10.8).

**Built (N104, ADR-7981).** The shared-port reader keeps one 64 KiB read buffer, drops a datagram over `MAX_DATAGRAM_BYTES`
uncopied (`dropped_oversize`), caps each half's queue at `MAX_QUEUED_BYTES` (4 MiB) as well as 4,096 datagrams, and counts
drops per source (`SharedPort::drops_by_source`, at most 1,024 sources).

**Designed, not built (wire 6, ADR-8858 A3-A5; lane v07-wire6-resume-server).** The resume register: per ticket id the
highest flight number seen and at most two pending resumes (each lives `PENDING_TTL_MS`), at most `max_peers` x 8 ticket
rows. A row leaves when its resume is confirmed or its ticket expires, never to make room; when the register is full a new
ticket's flight is refused to the full handshake (`resume_busy`). Built in part 1: the per-peer state that makes a ticket
single use without a row, eight bytes (`ResumeEpochs`), and a resumption ticket of a fixed 142 bytes.

**Built (wire 6 part 2, ADR-8902).** The register as designed (`regia_net::resume_register`): rows keyed by the ticket's
key id and nonce, at most `max_peers` x 8 (at least 8), each with the highest flight number seen and at most two pending
resumes; a flight above the highest takes a free place or the place of the lower flight; a pending resume holds one
finished link, its stored reply (at most 283 bytes), the hello's join and the claims, and lapses after `PENDING_TTL_MS`;
a row leaves at the confirmation or at the ticket's expiry and never to make room. A peer id issued for a held slot's
return and never confirmed takes no slot.

### 11.6 QUIC option edges

Start at 1,200 with discovery on; an oversized datagram is counted and the receive loop continues (adversarial rows 5, 6).

Built (N114, ADR-7946): QUIC sends check the connection's current payload limit.
Endpoints reserve at most 1156 bytes for Regia framing, allowing QUIC's maximum
short-header/AEAD/DATAGRAM overhead within a 1200-byte path. A transport size refusal
shrinks and reschedules uncommitted work without consuming a connection packet
sequence; only never-transmitted reliable fragments may be split. Ciphertext nonce
counters always advance. Oversized receives increment `datagrams_lost`, record
`quic_bind`, and continue, with a 1024-drop work limit per socket call. Existing
`net.quic_status` exposes the counters. No wire or settings format change.

Decided (ADR-10077, KI-NT-027): QUIC stays a loopback and LAN option through v1.0. The internet transports are sealed
UDP and Steam networking, which every gated soak row measures.

## 12. Versioning and formats

### 12.1 Wire versions

| Wire | Line | What changed | Old peer sees |
|---|---|---|---|
| 2 | v0.5.0 | baseline | - |
| 3 | v0.5.1 | reliable input lane (ADR-7896) | `protocol_mismatch` deny |
| 4 | v0.5.x | `LinkFeedback` (§ 6.4), quality report (§ 13.1) | deny + version probe answer |
| 5 | v0.6 | transport v2 (§ 4, § 5), lockstep, path challenge frame | deny + version probe answer |
| 6 | v0.7 | one-round-trip ticket resume (§ 10.3): the resumption ticket in the sealed `Accept`, packet kinds `ResumeInit` 11, `ResumeReply` 12, `ResumeRetry` 13; the `RUMSG` frame on stream 7 (§ 5.2, ADR-10052); the far-tier state record, kind 3 (§ 8.3, ADR-10124: transform record revision 4 in the protocol hash) | deny + version probe answer |

`REGIA_NET_WIRE_VERSION` feeds `protocol_id` (`payload.rs:12`, :105-111) together with the game, protocol and content versions, so
a game always deploys matched client and server builds. **There is no cross-wire compatibility path**: a game never needs one
engine version's client to talk to another's server, because its own game version is in the same id. What every version must do
is fail clearly, which the version probe guarantees.

Built (wire 6, ADR-8858): `REGIA_NET_WIRE_VERSION` is 6. It is the one wire step of the v0.7 line: every wire-shape change
of v0.7 is added to wire 6 before its freeze, never as a 7. A wire 5 peer is refused as the table says: its `ConnectRequest`
gets the `protocol_mismatch` deny (with the server's version stamp when the request carried one) and its version probe is
answered with wire 6 (`transport_v2::a_wire_version_5_peer_is_refused_cleanly_and_its_probe_answered`). Record
shapes above the transport ride the protocol hash inside wire 6: two wire 6 builds whose transform records differ (a
build without the far record is revision 3, with it 4) refuse each other with `protocol_mismatch`, and a dev build
names `regia_net::replication::transform_record (different)`
(`far_tier_record::the_far_record_is_transform_record_revision_4_in_the_protocol_hash`,
`quantized_transform::a_client_with_the_old_transform_record_is_refused_cleanly`). A wire 6 client
that opens with a `ResumeInit` against a wire 5 server, which drops the unknown kind, sends its `ConnectRequest` after three
unanswered flights and gets the same deny
(`resume_ticket::a_server_that_drops_the_resume_kind_gets_a_connect_request_and_its_deny_names_the_older_side`).

### 12.2 The version probe [D27]+[PUB]

A plaintext datagram any build answers on the game socket, frozen forever: request `RGVP | probe_version u8 | nonce u64 |
padding to 64 bytes`; answer `RGVR | probe_version | nonce | wire u32 | game_version str<=32 | protocol u32 | content_hash u64`
(at most 64 bytes, so no amplification), rate limited per source prefix. A client that gets `protocol_mismatch` sends a probe and
shows "the server runs <game version>, you have <game version>: update" naming which side is older (S6; today the deny cannot
say). v0.5.x builds that predate the probe simply do not answer; the newer side then says "the server is older than this build"
from the absence of an answer after a valid deny.

**Built (N109, ADR-7985).** `regia_net::version_probe`, format `version-probe` v1 (`docs/formats/version-probe.md`); per-prefix
limit 4 answers/s, burst 8, IPv4 /24, IPv6 /56, plus the server's global pre-proof cap; answered by `NetServer`, by the relay
outside a flow (forwarded to the host inside one) and routed by the shared-port reader. Refinement: a version stamp (`RGVS |
wire | protocol | content_hash | game_version`) rides in every `ConnectRequest`'s padding, and a stamped request gets the
server's stamp after its `protocol_mismatch` deny, so two builds from this one on name the older side without the probe's
round trip; the probe (resent every 250 ms, 1.5 s wait) serves servers that predate stamps. Old clients get the side in the
deny text (`... (client_older)`). Order: dotted `NetGame::GAME_VERSION`, then wire, then protocol; equal versions with another
content hash is `content_differs`. `NetLink` checks content through the stamp: the app's replicated schema plus
`LinkOptions::content_hash` (the protocol id is unchanged, which the shared-port routing needs).

### 12.3 API versioning

Additive where possible. A real break goes through `cargo xtask api-snapshot --update --breaking` with its ADR, and the old name
stays deprecated for one minor line (v0.6 keeps `Channel::Reliable`, mapped to the game RPC stream; `CongestionNotches`
deprecated; v0.7 removes them). New enum variants on public events are listed under Upgrade in each release fragment.

**Built (v0.7, ADR-10067).** Both names are removed. `Channel::GAME_RPC` (the constant for
`Channel::Stream(StreamId::GAME_RPC)`) replaces `Channel::Reliable`, and stream-2 messages are reported as
`Stream(StreamId::GAME_RPC)`; `congestion::LossNotches` replaces `CongestionNotches` with the same behavior (a loss-only
budget, not congestion control: sends are sized by the allowance of § 4.2). No wire change.

### 12.4 Formats

New: `*.desync.ron` v1 (desync bundle). Changed: `config/net.ron` (`NET_SETTINGS_FORMAT_VERSION` 2 -> 3: timeout policy,
controller floor, lockstep and migration settings) and `server.toml` (`SERVER_TOML_FORMAT_VERSION` 3 -> 4: relay caps, mtu
probing, rate floor), each with a migrator and a `regia upgrade-project` step (spec 12 § 4a), and listed in format-hashes. The
net-soak `result.json` is a report, not a content format.

## 13. Observability

### 13.1 `PeerQuality` [PUB]+[D27]

```rust
/// What the transport knows about one peer's link, for the game's HUD and its own decisions (spec 120 § 13).
pub struct PeerQuality {
    /// Smoothed RTT and its p50/p95 over the last 10 s, milliseconds.
    pub rtt_ms: f32, pub rtt_p50_ms: f32, pub rtt_p95_ms: f32,
    /// Interarrival jitter (RFC 3550 § 6.4.1), milliseconds.
    pub jitter_ms: f32,
    /// Loss we measured on what we sent, and loss the peer measured on what we sent (upstream for a client), last 10 s.
    pub loss_out: f32, pub loss_in: f32,
    /// Queueing delay over the minimum RTT, milliseconds.
    pub queue_delay_ms: f32,
    /// Rate the controller allows now and what was sent, bytes per second.
    pub send_allowance_bps: u32, pub sent_bps: u32,
    /// Input arrival margin p5 in ticks, late and missing input fractions (server-measured).
    pub input_margin_p5: i16, pub inputs_late: f32, pub inputs_missing: f32,
    /// Overall grade with hysteresis.
    pub grade: LinkGrade,
    /// Whether the link is interrupted now.
    pub interrupted: bool,
}
/// Good / Fair / Poor / Interrupted, with a 2 s hysteresis so the HUD does not flicker.
pub enum LinkGrade { Good, Fair, Poor, Interrupted }
```

Grades: `Good` (RTT p95 < 150 ms, loss < 2 %, missing inputs < 0.5 %), `Fair` (< 300 ms, < 5 %, < 2 %), `Poor` otherwise,
`Interrupted` per § 10.2. Events: `QualityChanged { peer, from, to }`, `PeerInterrupted`, `PeerRestored`, `RateLimited { peer }`
(the controller's target fell under the game's declared need for 2 s). A client learns its upstream loss and input lateness from
`LinkFeedback` (§ 6.4).

### 13.2 Operator telemetry [D27]

A `regia_net::stats` tracing row every 10 s per peer (RTT p50/p95, jitter, loss in/out, input margin, rate, bytes/s) and a server
row every 60 s (tick step and loop p50/p95/p99, sim share, RSS, CPU, kbps, peers by grade, desync and resync counts), off by
default for clients, on for dedicated servers; addresses redacted unless the operator opts in.

### 13.3 Agent protocol

New or extended methods (each with a schema row, dispatch arm, a dispatcher e2e test, a book/agents page and a COVERAGE row in its
build lane): `net.quality` (per-peer `PeerQuality`), `net.peers` gains `grade` and `interrupted`, `net.lockstep` (delay, frame
lag per seat, resyncs), `net.desync` (last verdict, probe rows) and `net.desync_inject` (test builds), `net.relay_status`
(allocations, caps, bytes), `net.migration` (state, successor), `net.rollback` (replays, depth, too-old). Error codes are lowercase
ascii (`rollback_too_old`, `desync_inject_disabled`, `relay_full`, ...), listed on each page.

**Built (v0.5.x, N110, ADR-8182; § 13.1-13.3 on `NetServer`/`NetClient`):** `regia_net::quality` and `regia_net::stats`.
Refinements: `PeerQuality` adds `rttvar_ms` and `loss_recv` (loss on what this end *received*, measured here): read
literally, `loss_out` and `loss_in` both describe what this end sent, so neither direction field covered a client's
downstream; on the server `loss_in` is 0 (no client loss report on wire 4). Windows are 10 s sampled every 250 ms; loss
counts from 32 resolved packets, input fractions from 30 ticks. The 2 s hysteresis is the recovery hold (with every signal
under 75 % of the better grade's limits); a worse grade shows after 500 ms in which the signals stayed past 75 % of the limit (a Schmitt band), so bad-wifi reads
`Fair`/`Poor` within 3 s (1.1-2.9 s over eight seeds); a link with nothing received since the last sample moves no grade;
`Interrupted` shows at once and the restore brings back the grade from before the outage, with the windows restarting 1 s
later. Jitter is reported but not graded (the thresholds above do not name it; RTT p95 carries it); the correction rate is
a prediction-path signal and waits for it. `LinkFeedback::upstream_loss` is now the server's 10 s `loss_recv`. The jitter
stamps reuse wire 4: `sent_ms u32` after each tick sync and a client `LANE_CLOCK` (5) every 250 ms. `queue_delay_ms`,
`send_allowance_bps` and `RateLimited` wait for the rate controller; the server row's CPU share and desync counts for the
lockstep lanes. `net.quality` and the `net.peers` `grade` are built; the other § 13.3 methods belong to their lanes.

**Built (v0.7 line, landed on integ-v061a, ADR-8889):** the above, ported onto wire 5. The clock stamps are additive: a
client sends `LANE_CLOCK` only after a tick sync that carried the server's stamp, so neither a v0.6 client nor a v0.6 server
sees anything it refuses; they ship under v0.7's wire 6. The socket drop counter is the base's `socket_drops` (ADR-8407;
the source lane's `socket_dropped` and `DatagramSocket::dropped_sends` are not landed). `queue_delay_ms` and
`send_allowance_bps` now read the rate controller (§ 4.2). A resume (§ 10.3) or a `NetLink` reconnect starts a fresh
connection grade, and the change back from `Interrupted` is reported on both ends. A loss fraction counts from 67 resolved
packets (was 32): with fewer, one lost packet reads as a loss rate past the `Good` band, and on wire 5 a clean good-wifi link
showed `Fair` for 2.5 s after it started. `RateLimited` is still not built (no declared need yet).

## 14. DO27's move onto it

DO27's own plan (N-spec) swaps the transport under today's lockstep in N1-N4, then moves to state sync (N5-N9). With this spec:

| DO27 step | What DO27 uses from `regia_net` | Engine lanes it needs (§ 17) |
|---|---|---|
| N1-N2 transport swap under its lockstep | `NetServer`/`NetClient` sealed (`CryptoMode::Required`, known hosts), ADR-7892 rejection, ADR-7896 large inputs, `TickDriver::ready_for` | v051 fixes; N101-N107 (timeouts, resume, rebind validation, reassembly, flood, decode, acks) |
| N3 relay | relay v2 with many clients per host | N112, N113 |
| N4 retire DO27's socket, probe, discovery, join-code helpers | socket buffers + DSCP, the version probe, configurable LAN announce, address join codes | N109, N204 (socket buffers, DSCP), N232 |
| Optional before N5: retire DO27's `HostLockstep` | `regia_net::lockstep` + `desync` (frames unreliable with redundancy, corroborated verdict, resync) | N224-N227 |
| N5-N6 state sync on the external sim | `ExternalReplication` over `SnapshotEncoder` (fixed), quantized records, distance tiers | N108, N230, N213, N214 |
| N7 late join and migration | resume for every game kind; lockstep re-host; successor journal | N216, N217, N225 (lockstep late join); N301-N303 |
| N7 prediction of its own step | `RollbackLog` | N223 |
| N8 Steam and lobbies | `SteamSocket`, lobbies, store ticket hook in `validate_join` | N219, N232 |
| N9 delete lockstep | - | - |
| Headless server without the ECS | the `ecs` feature off | N221, N222 |

What DO27 still fixes in its own shell while it keeps its lockstep (game rules, not engine work; for DO27's lead): the single-
report match end, the producer pack-up team check, uncapped command blobs and axis ranges, the 400 ms refusal sleep on the player
host's match thread, the unauthenticated relay bind while its old relay stays, and broadcasting every joiner's address.

### Built (N232, ADR-7954)

N4 and N8 helpers this lane ships: `DiscoveryConfig` (magic and port; the defaults stay `RGNA` and UDP 7900); address join codes beside the 6-character relay codes (IP, port, nonce, a 12 hour TTL, the existing 31-character alphabet; the host password stays the access control); `JoinTicketCheck`, called from `NetServer` admission after the password gate and `validate_join` and before the peer is added. A refusal is `ticket_refused`. No wire-version bump: the refusal travels as `DenyReason::InvalidJoin` with detail `ticket_refused`, and `ClientEnd::as_str` reports `ticket_refused`. Steam's check is `regia_steam::SteamJoinTicketCheck`. The live `BeginAuthSession` path is behind the `steamworks` feature; tests use an in-memory table and make no network call.

Difference from this section's wording: the section names a store ticket hook in `validate_join` and does not assign a wire byte. This lane calls the hook from admission beside `validate_join`, not from inside that trait method, so a game's `validate_join` stays a content check. Wire 4 and wire 5 belong to other lanes, so the refusal does not take a new deny byte.

Fix (ADR-7989): the hook runs after the live-peer, per-address and capacity checks, and the server ends the store session when the peer leaves. `BeginAuthSession` ok is a pending admit; `ValidateAuthTicketResponse` can kick later. `LanSocket` binds the discovery port with `SO_REUSEADDR`. Address-code nonce and expiry stay advisory, checked by the decoding client. `MAX_JOIN_DATA_BYTES` stays 512.

Fix (ADR-8502): late refusals are applied in `NetServer::receive`, so the ECS loop kicks too. A Steam refusal counts only while that session is open. `DuplicateRequest` for a session the hook holds is a restart on a new address: the old session ends, the stale peer is kicked, and the new ticket is pending. `LanSocket::poll` returns the sender with each announce and reads at most 256 datagrams per call. A dormant player keeps its store session until the resume hold ends.

Fix (ADR-10069, KI-NT-047): the hook installs on every server type. `NetSession::set_join_ticket_check` (and
`SessionServer::set_ticket_check`) checks the hello's `auth` bytes (`ConnectOptions::auth`) after the password, on resumes
too; in-process loopback and simulated clients are not checked. `Refuse` answers `auth_failed` with `ticket_refused`, a
late refusal kicks with `ticket_refused`, and `end_ticket` runs once the player is gone (not during grace).
`ServerHost::set_join_ticket_check` hands the hook to the dedicated host's session. `NetLink::set_ticket_check` installs it
on a link server, and `NetLink::connect_with_join` sends the ticket as the game's join data. No wire change.

## 15. Testing

### 15.1 Profiles

The harness's eight (clean, good-wifi, bad-wifi, mobile-4g, transatlantic, congested, flappy, hostile; ADR-7899) and the audit's
nine (high-rtt, rtt-step, clock-drift, nat-rebind, cgnat, wifi-to-cell, long-outage, bufferbloat, downstream-loss), plus:

| Profile | Numbers (per direction unless noted; seeded) | Why |
|---|---|---|
| asym-bufferbloat | uplink good-wifi; downlink 4 Mbit/s with a 1 s drop-tail queue kept full by seeded cross traffic offered at 4.4 Mbit/s (a bulk download in the same home) | F2 (inputs late when only the downstream is queued) |
| listen-uplink | host 20 Mbit/s up shared by all clients, 128 KiB queue; clients good-wifi | F5 |
| lossy-fat-pipe | 100 Mbit/s, 5 % random loss, no queue, 20 ms | F7 (random loss is not congestion) |
| rtt-extreme | 1,500 ms one-way, 20 % loss, for 10 min on one peer of 8 | never removed for ping or loss |
| mtu-1280 / mtu-blackhole | path MTU 1,280; then 1,200 with every larger datagram silently dropped from t = 30 s | T2, PMTUD black-hole detection |
| flood | good-wifi + one peer sending 100k datagrams/s of valid sealed records | H5 |
| forged-handshake | 100k/s forged `ConnectRequest` from 1,000 /24s | H1b |
| relay-path | every client through the relay, good-wifi each leg | S5, relay caps |
| lockstep-mix | 8/16/40 seats, mix of good-wifi, bad-wifi, mobile-4g, transatlantic | L1 |
| mixed | 40 % good-wifi, 25 % bad-wifi, 15 % mobile-4g, 10 % transatlantic, 5 % flappy, 5 % congested | R4 scale bars |

**Built (N100 net-soak-profiles-v2, ADR-8030).** All twenty are in `regia_net_soak::profiles` with the numbers above.
Refinements: numbers the table leaves open take good-wifi's; `lossy-fat-pipe`'s "no queue" is a 64 KiB buffer (5 ms
at 100 Mbit/s), because a zero buffer would drop back-to-back datagrams as queue loss; `rtt-step` and `long-outage`
keep their times (60/120 s, 60 s) and the plans run them 180 s and 120 s; `cgnat` uses groups of ten (nine share an IP
from the start, the tenth moves in at 40 % of the run) and runs at least 10 clients; `wifi-to-cell` switches at 40 % of
the run; `rtt-extreme` puts one client in eight on the extreme link, whatever the run length; `flood`'s peer runs from
10 s for 10 s and is judged by the harness's hostile bars; `relay-path` and `lockstep-mix` are skipped with
`needs: relay v2` / `needs: lockstep`. The `rpc_p95_with_bulk` run is the variant `congested+bulk` (256 KiB to every
client at half the run).

**Amended (N224 second pass, ADR-8209).** `lockstep-mix` runs: the plan routes it to the lockstep workload
(`regia_net_soak::lockstep::run_lockstep`; its clients are the seats), 5 virtual minutes in the gate and nightly
plans, at 8, 32 and 40 seats in the gate. `lockstep-mix+action` is its variant on the action input mix (16, 24 and
40 seats). `lockstep-mix+bots` is the wander-fire bot mix (ADR-8409): one gate row at 40 seats for 600 s, and the
same 40-seat length in the nightly plan. Only `relay-path` is still skipped. The soak's clients stamp their inputs through the engine's own loop
(`NetClient::due_input_ticks`) and the kit judges against the engine's timeout policy
(`regia_net::TimeoutPolicy::default()`): `flappy`'s long outage is that removal timeout plus 2 s (22 s).

### 15.2 Bars (gate rows)

The harness's six bars stay (involuntary_disconnects, reconnect_within_grace, no_wedged_client, hostile_dropped,
others_unaffected, bandwidth_within_budget). Added, each a function of `result.json`:

| Bar | Pass when | Profiles |
|---|---|---|
| missed_input | <= 0.5 % good-wifi/clean, <= 2 % bad-wifi/mobile-4g, <= 3 % asym-bufferbloat | those |
| corrections | <= 1 % of predicted ticks on good-wifi, <= 3 % on bad-wifi | those |
| staleness_p99 | <= 4 ticks on every profile except congested (<= 8) and inside outages | all |
| rate_vs_bottleneck | offered <= bottleneck within 5 s, held | congested, bufferbloat, listen-uplink |
| queue_delay_p95 | <= 100 ms | bufferbloat |
| rpc_p95_with_bulk | <= 1.5x the no-bulk control | congested + bulk |
| random_loss_kept | the snapshot budget stays >= 90 % of the clean run | lossy-fat-pipe |
| interrupted_once | one Interrupted and one Restored per outage, none on clean links | flappy, long-outage, good-wifi |
| resume_same_peer | every resume keeps `PeerId` and entities | long-outage, wifi-to-cell |
| no_hijack | 0 address moves without a validated path | hostile, nat-rebind, cgnat |
| never_kicked_for_ping | the rtt-extreme peer stays the whole run | rtt-extreme |
| desync_zero | 0 desyncs, 0 resyncs; with injection, the injected one is named and resynced | lockstep-mix, lockstep-mix+action, lockstep-mix+bots |
| lockstep_upload | over a 5-minute run, host upload in the 95th-percentile second <= 4.0 Mbit/s and in the busiest second <= 4.4 Mbit/s: 40 seats on the order-driven mix and the bots mix, 24 seats on the action mix (§ 7.5, ADR-8209, ADR-8409) | lockstep-mix, lockstep-mix+action, lockstep-mix+bots |
| tick_p99 | server tick p99 within the docs/perf budget at 40 (gate) / 64 / 128 (nightly) | mixed |
| join_under_flood | >= 99 % joins within 5 s | forged-handshake |

**Built (N100, ADR-8030).** Each bar is `regia_net_soak::report::spec120_bars` of a `result.json` entry (format v2;
v1 files load and are judged where their numbers allow), reported per run and non-gating; `xtask/src/net_soak.rs`
`BARS` lists each bar's owning lane, which flips it. Definitions the table leaves open: `missed_input` is the run-wide
fraction; `corrections` is per compared prediction; `staleness_p99` drops samples inside an outage; `rate_vs_bottleneck`
judges every whole second from 5 s after each connect at every finite bottleneck, 28 header bytes per datagram
included; `rpc_p95_with_bulk`'s control is the same run before the transfer; `random_loss_kept` compares with the full
world (a clean run's budget); `interrupted_once` counts outages of at least `interrupt_ms` (1,500 ms) that start while
connected; `no_hijack` counts moves to an address the client never held (the harness's ground truth); `tick_p99`'s
budget is one tick period until docs/perf has a row (N215).

The gate row stays off until every gate bar is green on the line (net-soak ADR-7899 step list), and is never turned on by moving
a bar. The gate set grows by the new profiles at 8 and 32 clients (target: under 60 s of wall time after the build); the nightly
set runs every profile at 8/32/64/128 clients and both modes.

**Built (N215 net-scale-soak, ADR-8454).** `tick_p99`'s budget is one share of the 30 Hz tick per tier (a quarter,
8,333 µs, up to 40 clients; a third up to 64; half above), the `net_scale_tick_p99_*` rows of `docs/perf/budgets.ron`;
the server tick is the soak's whole server iteration, so the rest of the tick is the game's. The § 1.1 scale row runs as
`cargo xtask net-soak --plan scale` (`mixed` and `mixed+hostile` at 40/64/128 on a dedicated server, `listen-uplink`
at 16/32 on a listen server, 180 s; `scale-gate` is the 40/32 set at 60 s), with `--wire udp` for real sockets on
127.0.0.1; "one hostile peer" is the `+hostile` variant. Each run records allocations per tick, server CPU per core,
Mbit/s per seat, budgets per client, end causes and RSS. On the build server at the lane's base: 40 clients on `mixed` keep
every player within budget with tick p99 4.9 ms and 6.7 % of one core (13.2 Mbit/s up, 330 kbit/s per seat); 64 hold
their tier (9.0 ms); 128 keep every player but miss their tier (24.5 ms against 16.7 ms: the sample game's world grows
with its avatars, so the tick is about 1 µs per client-entity send); listen-uplink at 32 is clean. The hostile peer at
40 is dropped only once N103 and N105 are on the line (its test is ignored until then). The two-machine release check
of § 15.3 is unchanged.

**Built (v07-net-scale-128, ADR-10081).** The `tick_p99` tiers are unchanged and all three are met on a quiet core
(§ 1.1 Built note). Two tests count the work instead of timing it: `connection::scale_work_tests` (a poll examines only
sent packets whose fate is open) and `scale::the_server_allocates_within_its_budget_per_client_per_tick` (at most 30
allocations per client per tick at 40 clients on `mixed`; 22.5 measured, 41.8 before).

**Built (v07-net-soak-wedged-hole, ADR-8887).** `no_wedged_client` judges every input gap the server sees up to the
peer's removal (not only up to its next input or the end of the run), clipped to the client's own connections as that
peer; an outage a gap overlaps excuses itself and `INPUT_GRACE_MS` (1 s) after it, never the rest of the gap, on the
server's input gaps and on the client's snapshot and send gaps. `Knobs::silent_inputs` / `Knobs::silent_snapshots`
pin it: a client that sends nothing, or hears nothing, while connected fails the bar. A never-heard peer's
gap starts at its join grace (`INPUT_GRACE_MS` after the join) also when its first input closes it, as a removal or
the run's end already did (ADR-10107).

**Built (v0.7, v07-net-staleness-rtt, ADR-10096).** `staleness_p99` as built counts the client's estimate of the server
tick minus the tick of its newest snapshot, sampled every client frame with the link up: the age of the client's newest
state, with the path's one-way latency inside every sample (a path of `d` one way reads at least `floor(d / tick) + 1`
ticks with a sender that adds nothing). The table's limit of 4 ticks therefore cannot hold above about 60 ms one way at
30 Hz, and the bar is red by its definition on `high-rtt`, `rtt-step`, `wifi-to-cell` and `rtt-extreme`. Each run now
also reports the same samples with the path's whole ticks taken off and the mean of each part of a sample (path,
transit, hold, rounding, estimate): `measures.staleness_paths`, and a table in `summary.md`. On the gate rows
the four profiles read 2 to 4 ticks beyond the path, inside the limit (`high-rtt` 3, `rtt-step` 3, `wifi-to-cell` 4,
`rtt-extreme`'s peer 2 to 3); what is added is about 1.5 ticks on average (0.47 for counting in whole ticks, 0.3 to 0.5 the
snapshot interval, 0.2 to 0.5 transit, 0.2 to 0.4 the tick estimate), and nothing is held by the sender.
The bar, its limits and its grouping are unchanged; ADR-10096 asks the lead for a ruling on judging the path-free
number.

### 15.3 Real-process soak [D27]

A release check (not a gate row): one dedicated server and N bot client processes over real UDP between two machines through the
harness's impairment shim on both ends, each preset for 3 minutes, plus a 30-minute RSS soak (growth <= 20 %); record server CPU,
RSS and Mbit/s per seat at 40 seats, as DO27 did, in `docs/perf/`.

### 15.4 Unit and scenario tests

Every lane's acceptance tests run inside `cargo test` on the crate's in-process conditioner or the harness library (seeded,
virtual time, no sleeps, no wall-clock asserts). DO27's scenario list becomes engine tests in the lanes that own each area:
N-seat lockstep five minutes with 0 desyncs (through the relay too); 600 ticks of agreeing subsystem hashes at 4 seats; an
injected desync named by the next hash tick; stalls 3 s / 8 s keep, 12 s ejects; silent seat never blocks; leave and GG on the
same tick everywhere; late input under 0-300 ms with bursts, blackout and duplicates loses no press; delay raise and lower switch
on the same tick; the migration list (host drops at several ticks with 3 and 8 peers, the winner dies too, unequal ticks resume
at the highest, catch-up larger than the stream cap, the agreed port taken, the old host rejoins as a player, migration under
reorder and loss, two in a row, journal depth edge); 3,000 ms RTT 20 % loss never removed; 1,000 reliable messages in one tick
capped per class; 10 % loss for 10 s then clean recovers within 5 s; 30 fps vs 300 fps same bytes within 2 %; an old build gets
a readable update answer; 20,000 seeded mutations of each resume and migration payload decode without a panic and a counting
allocator bounds every length field.

**Built (N206, ADR-8185).** Tests of prediction, clock and lead under drifting clocks, sub-tick arrival and changing
round trips use `regia_net_soak::timed::TimedHub` (seeded, virtual microseconds, the real transport and its RTT
estimator). `regia_net::testing::LoopbackHub` stays for exact per-frame scripting (whole-tick delays, frame filters).
The parity run of `no_false_corrections_under_latency_jitter_and_loss` on the time harness is in
`crates/regia_net_soak/tests/time_harness.rs`.

### 15.5 Fuzzing and replay

Stateful libFuzzer targets (registered in the fuzz-target registry, `fuzz_targets` test kept green): out-of-order fragments and
stream windows, a sealed endpoint fed a sequence of datagrams, the shared-port reader, rebind and path-validation sequences, the
relay allocation table, `VoiceRelay::route`, the `SnapshotEncoder` decoder, lockstep frames, desync bundles, the version probe.
Client capture with timing (spec 103 extension): a client recorder that logs datagram arrival times and local inputs, so a
bad-wifi soak failure replays to the same corrections. Gate checks: `cargo check -p regia_net --no-default-features` and the
no-ECS dependency assertion (§ 3.2).

**Built (N220, ADR-8468, net-fuzz-coverage).** Structure-aware op scripts instead of framed datagram lists, and models
written from the spec instead of decoder round trips: `net.connection_sequence` (+ `.hostile`, the E6 bomb) and
`net.sealed_endpoint_sequence` (+ `.hostile`; a real Noise handshake, replays, rebinds with the probe acknowledged from the
new port, forged and 64 KiB datagrams) check
each packet against a reliable-stream model with the § 5.2 deadline as a band (must fail past 4 s of traffic counted at the
minimum RTO, must not fail under 2 s counted at the maximum); `net.shared_port_sequence` checks every datagram against a
model of the routing rules; `net.snapshot_sequence` checks every decoded delta against the world encoded at its sequence
number under drops, reorders and duplicates (a late one must leave the shown world unchanged, ADR-7983); `net.voice_relay_route`; `net.input_bundle_budget`. Run on the line before the reassembly caps,
the `.hostile` fixtures and mutations of the honest scripts are findings (the gap stands past every deadline, an over-long
fragment is accepted, a 64 KiB datagram reaches a shared-port half). Already on the line: `net.snapshot_delta`,
`net.version_probe`, `net.relay.allocate`/`refresh`. Still to add with their code: stream windows, path-validation frame
sequences, lockstep frames, desync bundles, QUIC (behind `net-quic`, which the registry does not build).

**Built (net-core-no-ecs-b, ADR-8501).** `cargo xtask check-net-core` runs both gate checks plus the core's unit tests and the
`net_core_headless` two-client session without `ecs`; `cargo xtask preflight` runs it after `check-server-build`, CI runs it,
and `check-layers` repeats the dependency assertion.

**Built (N231 net-client-capture-replay, ADR-8221): client capture with timing.** `regia_net::capture` (core) and the
`NetClient` tap and replay of spec 103 § 14: decrypted packets, receive and flush times, input stamps, ends and the game's
frame digests in a byte ring allocated once, saved as `.netcapture` v1. The net soak's client writes a frame marker with a
chained digest of its output (corrections, predictions, every message it was handed) after each frame;
`regia_net_soak::workload::replay_client` rebuilds a client from the scenario in the capture's label and replays it at the
captured frame times. A failing soak run is rerun with a capture on every client (`capture_ring_bytes`: 96 KiB per virtual
second, 8-128 MiB) and writes `client-<i>.netcapture` under `REGIA_NET_SOAK_CAPTURE_DIR` (else `captures/` in
`REGIA_NET_SOAK_OUT`), printing `REGIA_NET_SOAK_REPLAY=<file> cargo test -p regia_net_soak --test capture_replay --
--ignored --exact replay_capture_file --nocapture`. Refinement: the replay compares rather than injects inputs (the soak
client's inputs are seeded, so a matching input record proves the stamps); an ECS world's replay through `NetLink` is a
follow-up.
The capture also holds the handshake's shortest round trip in front of the accept (record kind 11, integ-v07-net,
ADR-10231): since ADR-10096 it anchors the client's clock and startup lead, and a replay, which runs no handshake, stamped
its first inputs a tick lower without it.

**Built (v07-net-land-capture-replay, ADR-8890): capture and replay through `NetLink`.** `NetLink::set_capture` keeps one tap
across reconnects and resumes, `NetLink::capture_frame` writes the game's frame digest, and `NetLink::replay` /
`replay_on` feed a captured session to a fresh ECS world (bridge, replication, prediction, interpolation) at the captured
link-clock times, sending nothing; `NetLink::replay_progress` counts receives off the captured times. The time harness
captures a client of a virtual-time session (`TimedHub::capture_client`) and replays it (`timed::replay_client`): a bad-wifi
session with prediction and interpolated proxies replays to the same chained digest after every update
(`regia_net_soak` test `link_capture_replay`). Agent: `net.capture_start`, `net.capture_save` (book/agents/net-capture.md).
No wire change.

## 16. Checklist: every row, better than both

"Today R" is Regia v0.5.0 (the audit's verdict), "Today D" is DO27's live path (the study's), "Target" is this design, "Bar" the
test that proves it (§ 15), "Lane" the § 17 lane. Every target row is at least as good as the better of R and D, and better than
both on the row's state-of-the-art criterion (tested under a bad network), except where marked out of scope.

| Row | Today R | Today D | Target | Bar | Lane |
|---|---|---|---|---|---|
| T1 congestion control | MISSING | none | GCC-family controller + pacing + aggregate budget + floor (§ 4.2) | rate_vs_bottleneck, queue_delay_p95, random_loss_kept | N202 |
| T2 path MTU | 1200 fixed | 1200 fixed | 1200 floor + opt-in DPLPMTUD + socket-drop accounting (§ 4.3) | mtu-blackhole recovery, no silent drop | N205 |
| T3 fragmentation on every channel | WRONG (fixed v0.5.1) | reliable only path | streams fragment, unreliable refused at the API with the limit queryable, producers split (§ 5.4) | no_wedged_client | v051 lanes, N203 |
| T4 no head-of-line blocking | one stream | one stream for everything | 8 engine streams + game streams, priorities (§ 5) | rpc_p95_with_bulk | N203, N204 |
| T5 loss detection and backoff | fixed 2 s, no backoff | fixed 250 ms | RFC 9002 thresholds, PTO with backoff, clean RTT samples (§ 4.1) | E2 p95 <= 1.5x one-way, outage resend count | N101 (backoff), N201 |
| T6 encryption | HAVE | forgeable | kept; election and relay permissions authenticated too (§ 10.5, § 10.6) | existing | - |
| P6 input redundancy | WRONG (fixed v0.5.1) | none (reliable) | unacked-only, delta, loss-aware count (§ 6.1) | missed_input | N207 |
| R1 deltas vs acked baselines | WRONG/PARTIAL | n/a live | fixed codec + quantized, error-gated records (§ 8.1-8.2) | <= 10 B per moving entity; reorder test | N108, N213 |
| R2 priority and budgets | PARTIAL | entity count | byte budgets from the controller, all records budgeted (§ 8.1, § 4.2) | bandwidth_within_budget, spawn burst test | N202, N212 |
| R3 interest at scale | PARTIAL | none live | per-profile radius, dirty cells, distance tiers, combat heat (§ 8.3-8.4) | 128 x 20k bench mean and p99 | N214 |
| R4 server at 64-128 | bench only | 40 seats measured | 64 gate-sized at 40, 128 recorded, real-process numbers (§ 15) | tick_p99, mixed | N215 |
| R5 large payloads | WRONG | no freeze class | shared limit, replication-control stream, RTT timeout (§ 8.1) | growing entity never frozen | N212 |
| R6 jitter buffer | PARTIAL | none | loss term, deferred-entity extrapolation (§ 9.7) | 80 % speed, extrapolation <= 1 % | N211 |
| P1 clock sync | WRONG (NetClient) | none | min-delay filter, never backward, slew (§ 9.1) | 0 backward steps | N111, N207 |
| P2 lead and dilation | WRONG | adaptive delay, merge late | margin-steered lead, fractional dilation, per-frame capture, merge late (§ 6.3, § 9.2-9.3) | press exactly once over 1,000 dilations | N209 |
| P3 prediction | WRONG at high RTT | none live | RTT-scaled history, continue after snap (§ 9.4) | high-rtt prediction stays on | N208 |
| P4 rollback for game state | MISSING | dormant | `RollbackLog` (§ 9.5), rollback lockstep (§ 7.7) | toy step reconciles exactly | N223, N304 |
| P5 lag compensation | WRONG default | 250 ms cap | per-shooter window, 400 ms default, claims clamped (§ 9.6) | 450 ms RTT shots exact | N210 |
| P7 replay | PARTIAL | lockstep replays, divergence tool | client capture with timing + desync bundles (§ 7.6, § 15.5) | soak failure replays | N227, N231 |
| S1 timeouts | WRONG | silence-only, tested | one policy, 20 s default, start grace, Interrupted, stall tests (§ 10.1-10.2) | interrupted_once, long-outage | N101 |
| S2 reconnect and resume | WRONG/PARTIAL | none | every game kind, live takeover, slot reclaim, auto reconnect (§ 10.3) | resume_same_peer | N102, N216, N217 |
| S3 connection migration | WRONG/HAVE | none | path validation, per-candidate budget, client side, CGNAT (§ 10.4) | no_hijack, cgnat | N103 |
| S4 host migration | MISSING | live lockstep re-host | lockstep re-host + journal for replication, authenticated, best connected (§ 10.5) | 16-player continues in 5 s | N301-N303 |
| S5 relay and NAT | PARTIAL | many clients, unsafe | allocations per client with permissions; ICE-style from the game socket (§ 10.6-10.7) | all NAT pairs join; 40 per host | N112, N113, N228, N229 |
| S6 versioning | PARTIAL | version probe | frozen probe naming both versions, side-specific deny (§ 12.2) | old build sees update | N109 |
| S7 Steam parity | WRONG resume | none | resume over Steam, receive caps, send failure surfaced (§ 10.8) | Steam soak 0 drops | N102, N104, N219 |
| H1a stateless handshake | HAVE | amplifiers | kept; probe and relay padded (§ 11, § 12.2) | existing | - |
| H1b join under flood | PARTIAL | none | per-prefix fair answers, IP-bound cookies (§ 11.4) | join_under_flood | N218, N114 |
| H2 spoofing and replay | WRONG (migration) | forgeable | path validation; relay permissions (§ 10.4, § 10.6) | no_hijack | N103, N112 |
| H3 malformed payloads | WRONG | one report ends a match | everything charged and named; corroborated desync (§ 11.2, § 7.6) | hostile_dropped | v051, N105, N106 |
| H4 reassembly bombs | WRONG | bounded | per-stream, per-connection, per-server caps; stall deadline (§ 5.2) | E6 capped | N104 |
| H5 flood fairness | MISSING | none inbound | pre-AEAD per-peer bucket, round-robin receive (§ 11.3) | flood bar | N105 |
| H6 fuzzing | PARTIAL | mutation loops | stateful targets (§ 15.5) | targets reach E6 at base | N220 |
| H7 impairment testing | PARTIAL | real-process (dormant path) | harness + new profiles + real-process release soak (§ 15) | gate row on | N100, N305 |
| Q1 quality signals | PARTIAL | link rows | `PeerQuality`, grades, events, upstream feedback, operator rows (§ 13) | client sees upstream loss within 2 % | N110, N111 |
| L1 lockstep mode | MISSING | HAVE live | unreliable redundant frames, budgeted delay, equal host delay, late join (§ 7) | lockstep-mix bars | N224, N225 |
| L2 desync | MISSING | HAVE live | corroborated verdict, resync, bundle, replay tool (§ 7.6) | desync_zero, injection named | N226, N227 |

## 17. Plan

Ordered by "keeps players connected and smooth" first. Each lane is 3-6 h, starts by writing acceptance tests that fail on its
base under the named profiles, and ships its agent surface, book page, provenance rows and fragment. The briefs are in the build machines
queue (`queue/staged/net-sota/`), one file per lane, generated from one table so they agree with this spec.

- **Phase 1, v0.5.x fixes** (no wire change, except N111's single bump to wire 4): N100-N114.
- **Phase 2, v0.6** (transport v2 = wire 5; time; resume for every kind; DO27's needs: core split, rollback log, lockstep,
  desync, external replication; NAT traversal): N201-N232.
- **Phase 3, v0.7** (host migration, rollback lockstep, the full gate row; the v0.7 integrator removes the names deprecated in v0.6, § 12.3): N301-N305.

"v051 fixes" = the running v0.5.1 lanes v051-net-input-rejection (ADR-7892) and v051-net-input-mtu (ADR-7896). "Replaces"
names the net-sota-audit brief (01NN) or held DO27 lane (hold/NN) each lane supersedes; the design lanes 0103, 0105, 0121,
0122, 0127, 0128 and hold/09, hold/13 are replaced by this spec and their build splits below. The briefs are the source of
truth for each lane's details; this table and the briefs are generated from one source so they agree.

| Id | Lane | Phase / line | h | RELEASE AFTER | Replaces | Acceptance in one line |
|---|---|---|---|---|---|---|
| N100 | net-soak-profiles-v2 | 1 / v0.5.x | 3-4 | net-soak-harness | new | each new profile's model has a unit test (seeded fate, numbers as the spec states: e.g. clock-drift +0.5 % gives 3,000 extra virtual ms over 10 min; asym-bufferbloat queue delay reaches 900-1,000 ms); each new bar evaluated on the v0.5.0 plan output reports FAIL where the spec's evidence says the base fails (missed_input on bad-wifi, rate_vs_bottleneck on congested, no_hijack on hostile) and PASS on clean where it should; result v1 files still load |
| N101 | net-timeout-policy | 1 / v0.5.x | 3-4 | v051 fixes | 0101 | 25 s outage with timeout 30 keeps the player (NetSession and NetLink); flappy 0 disconnects; RTO backoff bounds resends; Interrupted/Restored once per outage; rtt-extreme peer never removed; 12 s stall removes exactly that peer |
| N102 | net-session-resume-fixes | 1 / v0.5.x | 4-5 | v051 fixes AND net-timeout-policy | 0102 | early return resumes the same PeerId; live-token takeover; same-address rejoin enters grace; Join refreshes liveness; slot reclaim; streams survive a 5 s outage; Steam resume over Steam |
| N103 | net-rebind-path-validation | 1 / v0.5.x | 4-5 | v051 fixes AND net-session-resume-fixes | 0104 | F1 hijack gone (no_hijack under hostile); cgnat 9th peer joins and stays; nat-rebind every 30 s 0 drops; per-candidate budgets; client-side server migration |
| N104 | net-reassembly-caps | 1 / v0.5.x | 3-4 | v051 fixes | 0106 | E6 bomb capped and dropped; legit 256 KiB under bad-wifi arrives; server-wide cap at 64 attackers; stalled reassembly fails named; shared-port and Steam receive memory bounded |
| N105 | net-flood-fairness | 1 / v0.5.x | 4-5 | v051 fixes AND net-reassembly-caps | 0107 | 100k/s flooder next to 7 players: their inputs and RTT within control, flooder dropped in 2 s; bad/good alternation dropped; F3 malformed stream dropped |
| N106 | net-swallowed-decode | 1 / v0.5.x | 3 | v051 fixes | 0108 | spectator input, malformed voice, client reliable decode error, server-only kinds: each charged or dropped and named |
| N107 | net-ack-poll-fixes | 1 / v0.5.x | 4-5 | v051 fixes | new | 64 one-byte reliable messages in one poll, all delivered, one reply: packets_lost 0, reliable_resends 0; 64 max-size fragments plus one unreliable [1,2,3]: the payload is sent by a later poll; a late ack after the loss timeout leaves rtt near the pre-stall sample and 60 Hz inputs stamped from the estimate + 2 are stored; mtu_bytes 1400 sealed: every state frame unreliable and within max_unreliable_bytes |
| N108 | net-snapshot-delta-fixes | 1 / v0.5.x | 3 | none | 0116 | reordered packet never moves current() back; ring overflow under budget loses no entity |
| N109 | net-version-probe | 1 / v0.5.x | 3-4 | v051 fixes | 0123 | old client vs new server and new vs old each get the side-specific reason through NetLink and NetSession; mismatched content refused on NetLink; probe bytes pinned; probe answer <= request size; per-prefix rate limit |
| N110 | net-quality-signals | 1 / v0.5.x | 3-4 | net-timeout-policy AND net-input-feedback | 0124 | good-wifi grade stays good 10 min; bad-wifi fair/poor within 3 s; client sees server-measured upstream loss within 2 %; flappy interrupted once per outage; downstream-loss on the right direction |
| N111 | net-input-feedback | 1 / v0.5.x | 4-5 | v051 fixes AND net-ack-poll-fixes | new | asym-bufferbloat: missing inputs <= 3 % (base about 50 %, F2); E3 harness: 0 backward estimate steps on good-wifi, bad-wifi, mobile-4g, rtt-step (base 12/28/29); missing inputs <= 0.5 % good-wifi and transatlantic, <= 2 % bad-wifi and mobile-4g; a wire-3 peer refused cleanly |
| N112 | net-relay-allocations | 1 / v0.5.x | 4-5 | v051 fixes | hold/09 | a second client binds while the first plays: Busy on the base, accepted after; 32 clients each reach the host and exchange sealed traffic; the 33rd over a cap of 32 gets Full; a forger from a third address cannot spend or block an allocation; v1 client against the v2 relay gets the v1 answer or VersionTooOld, never silence |
| N113 | net-relay-host-flow | 1 / v0.5.x | 4-5 | net-relay-allocations | hold/09 | 16 clients join a host behind a simulated symmetric NAT (SimNat) by join code through the relay and play 5 min on relay-path with 0 disconnects; NetSession::connect(JoinCode) succeeds (errors on the base); a v1 relay yields relay_too_old within the connect timeout; net.relay_status reports 16 allocations |
| N114 | net-handshake-quic-edges | 1 / v0.5.x | 3-4 | v051 fixes | new | Challenge to port A, KeyInit from port B with the same cookie: the join completes before connect_timeout_ms (base: ConnectTimeout); a forged challenge followed by the real one: the join completes; QUIC peer with max datagram 1,200 rejects a 1,300-byte payload and still delivers a smaller retransmission; a 3,000-byte then a 100-byte datagram in one receive: the 100-byte one is delivered |
| N201 | net-v2-acks-loss | 2 / v0.6 | 5-6 | net-ack-poll-fixes AND net-timeout-policy AND net-soak-profiles-v2 | new | E2 rerun: small reliable p95 <= 1.5x one-way delay at 5 % loss (base 3.5x); a 64-packet burst fully acked by one reply; 15 % reorder on bad-wifi counts 0 lost packets that were delivered; resend count per fragment during an 8 s outage <= 4 (PTO backoff); wire-4 peer refused cleanly |
| N202 | net-v2-rate-controller | 2 / v0.6 | 5-6 | net-v2-acks-loss | new | congested and bufferbloat: offered rate <= bottleneck within 5 s and held (E1 at the base: never); bufferbloat queue_delay_p95 <= 100 ms; lossy-fat-pipe: snapshot budget >= 90 % of clean (base: 8 of 140, F7); listen-uplink at 64 clients: 0 timeouts and staleness p99 <= 8 ticks (base F5); 10 % loss for 10 s then clean: floor within 3 s, back within 5 s; essential class never under the floor |
| N203 | net-v2-streams | 2 / v0.6 | 5-6 | net-v2-acks-loss AND net-reassembly-caps | new | E2b: small RPC p95 with a 256 KiB bulk transfer in flight on congested <= 1.5x the no-transfer control (base 96x); a lost bulk fragment never delays a stream-2 message (counter); per-stream cap breach is a named protocol error; ReliableUnordered delivers out of order when a gap exists |
| N204 | net-v2-scheduler-sockets | 2 / v0.6 | 4-5 | net-v2-rate-controller AND net-v2-streams | new | a 1 MiB bulk transfer plus inputs on congested: input arrival unchanged vs no-bulk control (missed_input within +0.2 %); WouldBlock counted as socket_drops, not sent; 30 vs 300 fps within 2 %; buffer read-back reported in net.status; 1,000 reliable messages in one tick capped per class |
| N205 | net-v2-pmtud | 2 / v0.6 | 3-4 | net-v2-acks-loss | new | mtu-1280: probing settles at 1,280 and never sends a larger data packet; mtu-blackhole (larger datagrams dropped from t = 30 s): back to 1,200 within 3 probe losses with 0 lost reliable messages and no stall over 1 s; probing off by default changes nothing on clean (byte-identical wire) |
| N206 | net-time-harness | 2 / v0.6 | 3-4 | net-soak-profiles-v2 | 0110 | hub with clock drift, sub-tick delivery, RTT steps, real resend path; prediction runs through the real RTT estimator |
| N207 | net-clock-filters | 2 / v0.6 | 3-4 | net-time-harness AND net-input-feedback | 0114 | 0 backward steps on every path; missing input <= 0.5 % good-wifi, <= 2 % bad-wifi; spike affects <= 8 samples; residual input loss <= 0.1 % on lossy-fat-pipe |
| N208 | net-prediction-high-rtt | 2 / v0.6 | 4-6 | net-time-harness | 0111 | 450/650 ms RTT: prediction stays on, corrections smoothed; startup lead right within 2 s |
| N209 | net-dilation-input-capture | 2 / v0.6 | 3-4 | net-time-harness | 0112 | 1,000 dilation events: every one-step press applied exactly once; no 0/2-tick hitch; 8-packet burst kept; late inputs merged with presses kept |
| N210 | net-lagcomp-window | 2 / v0.6 | 3-4 | net-time-harness | 0113 | shots posed at the carried view tick at 450 ms RTT with defaults; stale claims clamped to the shooter's own age |
| N211 | net-interp-gaps | 2 / v0.6 | 3-4 | net-v2-rate-controller | 0115 | budget-deferred constant-speed entity never below 80 % speed; bad-wifi extrapolation <= 1 % of frames |
| N212 | net-replication-oversize | 2 / v0.6 | 4-5 | net-v2-rate-controller AND net-v2-streams | 0117 | entity growing past 16 KiB never frozen; no duplicate oversize records at 650 ms; baselines advance at 600 ms; spawn bursts within the budget |
| N213 | net-quantized-transform | 2 / v0.6 | 4-6 | net-replication-oversize | 0118 | <= 10 B per moving entity update at default precision; position error <= the precision; wire-mismatched client refused cleanly |
| N214 | net-relevance-scale | 2 / v0.6 | 4-6 | none | 0119 | 128 x 20k bench with a 1 km profile and 64 spawns/tick within new mean AND p99 budgets; distance tiers; combat-heat input raises priority |
| N215 | net-scale-soak | 2 / v0.6 | 4-5 | net-relevance-scale AND net-v2-rate-controller AND net-timeout-policy | 0120 | 40 clients mixed: 0 involuntary disconnects, tick p99 in budget, hostile dropped; 64/128 recorded; listen 32 on listen-uplink clean |
| N216 | net-link-resume-dormant | 2 / v0.6 | 5-6 | net-session-resume-fixes | new | long-outage 11 s and 45 s: the player keeps its PeerId and every owned entity (base: new PeerId, entities gone); a third client sees NetInterrupted during the outage and none after; the first frame after resume is a full state; grace expiry calls the hook once |
| N217 | net-link-resume-reconnect | 2 / v0.6 | 4-5 | net-link-resume-dormant | new | long-outage 25 s past a 20 s timeout: the client reconnects by itself and resumes the same PeerId within 2 s of the link returning; wifi-to-cell: 0 lost slots over 20 runs; Steam (mock) reconnect uses Steam; events fire once per outage |
| N218 | net-handshake-availability | 2 / v0.6 | 4-5 | net-flood-fairness | 0109 | 100k/s forged flood from 1,000 /24s: real client joins within 5 s in >= 99 % of 200 trials |
| N219 | net-impaired-steam-mock | 2 / v0.6 | 3 | net-session-resume-fixes | 0125 | Steam mock with seeded impairment; bad-wifi 5 min 0 drops; Steam resume same PeerId |
| N220 | net-fuzz-coverage | 2 / v0.6 | 3-4 | net-reassembly-caps | 0126 | stateful targets reach the reassembly bomb at the base; fuzz_targets green after |
| N221 | net-core-no-ecs-a | 2 / v0.6 | 4-5 | none | hold/13 | `cargo check -p regia_net --no-default-features` succeeds (fails on the base: regia_ecs is not optional); default-feature api-snapshot unchanged; every existing regia_net test passes with default features |
| N222 | net-core-no-ecs-b | 2 / v0.6 | 4-5 | net-core-no-ecs-a | hold/13 | the dependency assertion fails on the base and passes after; the headless example builds and runs a 2-client loopback session in a test without the feature; preflight runs the check |
| N223 | net-rollback-log | 2 / v0.6 | 4-5 | none | hold/14 | a deterministic toy step (integer position + velocity) reconciles to exactly the reference trace after a one-tick server correction; a correction older than the ring returns rollback_too_old and changes nothing; replay calls the step exactly (newest - server_tick) times; the visual offset reaches zero after N ticks and is monotonic; a late input amended for a past tick is used in the corrected run |
| N224 | net-lockstep-core | 2 / v0.6 | 5-6 | net-input-feedback | new | 4 seats on bad-wifi for 5 min: no stall over 2 ticks from a single lost frame datagram (base: no lockstep; DO27-style reliable frames stall a full resend); delay raise and lower switch on the same tick on every seat and no tick is stepped twice; lockstep-mix 40 seats: host upload <= 4 Mbit/s; host input delayed equally when Delayed |
| N225 | net-lockstep-sessions | 2 / v0.6 | 4-5 | net-lockstep-core | new | DO27 scenarios: a silent seat never blocks the frame; a clean leave never stalls; a killed seat keeps the match ticking; leave and GG land on the same tick for every seat; a slow loader inside the grace is kept; 3 s and 8 s stalls keep the seat, 12 s past a 10 s timeout ejects it; a seat reconnects after 15 s and is in step within 5 s with the same seat id |
| N226 | net-desync-detect | 2 / v0.6 | 4-5 | net-lockstep-core | new | 4 seats agree on every subsystem hash for 600 ticks; an injected desync on one seat at tick T is detected by the next hash tick, the probe names the subsystem, the seat is resynced and the match continues; one seat that sends forged hashes is resynced then removed, the others never stop; lockstep-mix desync_zero |
| N227 | net-desync-diagnose | 2 / v0.6 | 4-5 | net-desync-detect | new | an injected desync's bundle replays offline to the same tick and subsystem; a bundle from a different game version is refused with a readable error; 20,000 seeded mutations of the bundle decode without a panic and a counting allocator bounds every length field; the CLI and agent coverage tests are green |
| N228 | net-nat-candidates | 2 / v0.6 | 5-6 | net-relay-host-flow | new | every NAT type pair of SimNat joins (base: no production punch path); relay fallback nominated within 1.5 s on symmetric/symmetric; relay-path profile 5 min 0 disconnects; checks paced (no more than the spec's rate) |
| N229 | net-nat-hosting-flow | 2 / v0.6 | 4-5 | net-nat-candidates | new | against in-process PCP/NAT-PMP/UPnP responders, the race picks the first success and renews; the self-check reports unreachable when the mapping is removed; the capacity estimate on listen-uplink is within 20 % of the true per-client budget count |
| N230 | net-external-replication | 2 / v0.6 | 5-6 | net-snapshot-delta-fixes AND net-relevance-scale | new | a 2,000-entity external sim with 16 clients on bad-wifi: every client converges, budget held per client, far entities at their tier rate; no regia_ecs in the test's dependency closure (with net-core-no-ecs landed) |
| N231 | net-client-capture-replay | 2 / v0.6 | 4-5 | net-time-harness | new | a seeded bad-wifi soak run's client capture replays to the same correction count and output digest; a one-byte change to a captured datagram changes the digest; capture memory bounded by its ring |
| N232 | net-do27-retire-helpers | 2 / v0.6 | 3-4 | none | new | two announcers with different magics on one port do not see each other; an address code round trips and expires at its TTL; a join with a ticket the hook refuses is denied with `ticket_refused` |
| N301 | net-migration-lockstep | 3 / v0.7 | 5-6 | net-lockstep-sessions AND net-link-resume-reconnect AND the owner's answer to question Q1 (host migration) | new | DO27's migration scenarios: host drops at several ticks with 3 and 8 peers; the winner dies too; peers at unequal ticks resume at the highest; catch-up larger than the stream cap; the agreed port taken; the old host rejoins as a player; under reorder and loss; two migrations in a row; a non-member cannot win or join the election |
| N302 | net-migration-journal | 3 / v0.7 | 5-6 | net-link-resume-reconnect AND net-migration-lockstep | new | a successor's shadow world equals the host's world digest at every full-state point on bad-wifi; journal lag <= 2 ticks p99 on good-wifi; journal bandwidth within its budget share |
| N303 | net-migration-handoff | 3 / v0.7 | 5-6 | net-migration-journal | new | a 16-player listen match continues on the successor within 5 s with the same PeerIds and every entity present; the first successor dead too: the second takes over within 8 s; good-wifi and bad-wifi |
| N304 | net-rollback-lockstep | 3 / v0.7 | 5-6 | net-rollback-log AND net-lockstep-sessions | new | 2 seats on transatlantic: local input applied with the configured delay (1-3 ticks), rollbacks <= 8 ticks, the confirmed state equals a lockstep reference run bit for bit; 4 seats on bad-wifi: no desync over 10 min |
| N305 | net-soak-gate-full | 3 / v0.7 | 4-5 | every phase-1 and phase-2 lane that owns a gating bar | new | the gate row is on and green on the line (base: off, failing); the nightly plan runs every profile; the real-process soak runs on two hosts and records its numbers |

Critical path for "players do not get dropped": N101 -> N102 -> N103 (v0.5.x), then N201 -> N202 -> N203 (v0.6). Critical
path for DO27's flag day: the v051 fixes, N104, N105, N112 -> N113, N109 (v0.5.x); N221 -> N222, N223, N224 -> N226 (v0.6).
Roughly 52 lanes, about 230 lane-hours (202-254); with the build machines's usual 6-10 parallel net-safe lanes (the crate's hot files serialize
some of them: `server.rs` lanes N102/N103/N105 and `connection.rs` lanes N107/N201-N204 run in RELEASE AFTER order), phase 1
is about 2-3 calendar days, phase 2 about 6-8, phase 3 about 3.

## 18. Corrections to existing specs

- Spec 70 § 1 says the transport supplies congestion control and four channels; at v0.5.0 it has two physical channels and no
  congestion control. Transport v2 (§ 4, § 5) delivers both; until it lands spec 70's claim is wrong (addendum appended there).
- Spec 77 § 7.4 ("owns its entities again" after reconnect) holds only for session-only games until § 10.3 lands (addendum
  appended there).
- `book/systems/snapshot-delta.md`'s recommendation of `SnapshotEncoder` for an external sim needs the R1 fixes (lane N108).

## 19. Provenance (for the build lanes' `docs/provenance.md` rows)

| Algorithm | Public source | Lane |
|---|---|---|
| Packet number encoding, ack ranges, ack delay, ack frequency | RFC 9000 § 13, § 17.1, § 19.3, Appendix A | N201 |
| Loss detection thresholds, PTO, persistent congestion, pacing | RFC 9002 § 5-7 | N201, N202 |
| Delay-gradient overuse detector, remote-rate state machine, loss-based bound | draft-ietf-rmcat-gcc-02 (Holmer, Lundin, Carlucci, De Cicco, Mascolo) | N202 |
| Delivery-rate estimation, min-RTT window, bounded startup | Cardwell, Cheng, Gunn, Hassas Yeganeh, Jacobson, "BBR: Congestion-Based Congestion Control", ACM Queue 14(5), 2016 | N202 |
| UDP congestion floor and circuit breaker | RFC 8085 § 3.1; RFC 8083 | N202 |
| Max-min fair share | Bertsekas and Gallager, "Data Networks" (1992), § 6.5 | N202 |
| Streams and flow control | RFC 9000 § 2-4 | N203 |
| DPLPMTUD | RFC 8899 | N205 |
| Clock filter | RFC 5905 § 10 | N206, N207 |
| Server-guided time dilation, unacked input redundancy | T. Ford, "Overwatch Gameplay Architecture and Netcode", GDC 2017 (public talk) | N207, N209 |
| Rollback with input prediction | the GGPO rollback model as publicly documented; G. Fiedler, "Networked Physics" (2014) | N223, N304 |
| Deterministic lockstep | P. Bettner and M. Terrano, "1500 Archers on a 28.8", GDC 2001; G. Fiedler, "Deterministic Lockstep" (2014) | N224 |
| Quantization, smallest-three rotation | G. Fiedler, "Snapshot Compression" (2015) | N213 |
| Path validation | RFC 9000 § 8.2, § 9 | N103 |
| Relay allocations and permissions | RFC 8656 § 5, § 9 | N112 |
| Candidate gathering, connectivity checks, nomination | RFC 8445; RFC 8489; Ford, Srisuresh and Kegel, USENIX 2005 | N228 |
| Port mapping | RFC 6886, RFC 6887 | N229 |
| Interarrival jitter | RFC 3550 § 6.4.1 | N110 |
| Burst loss model | Gilbert (1960), Elliott (1963) | N305 |
| DO27 designs (lockstep host rules, desync probe, re-host, journal, timeout rule A5, anti-patterns A1-A11, version probe, telemetry rows, quantization and tiers) | the owner's DO27 designs, read per spec 25 § 2; code only from P3 files with a row naming crate, file and commit | each lane that adopts one |

## 20. Owner questions (recommendations; the planning report has the reasoning)

| # | Question | Recommendation |
|---|---|---|
| Q1 | Host migration for listen servers? | Yes, v0.7: lockstep re-host first, then replication games via the successor journal; dedicated servers do not migrate |
| Q2 | Relay infrastructure (money)? | The free self-hosted relay binary (community-run) plus Steam's relay network on Steam builds; no paid fleet before v1.0 (a 32-client match is about 3.9 GB each way per hour through a relay; one small VPS covers the demo if needed) |
| Q3 | Player-count targets? | Dedicated 64 on the mixed profile (gate-sized at 40), 128 recorded; listen 16 on the min-spec Windows laptop and 32 on a 20 Mbit/s uplink; lockstep 40 seats under 4 Mbit/s host upload |
| Q4 | Worst network we keep playable? | 650 ms RTT, 5 % loss, 60 ms jitter; timeout 20 s; resume grace 60 s; never removed for ping or loss |
| Q5 | Lag-compensation fairness? | Per-shooter window, 400 ms default cap, project-settable to 1,000 ms |
| Q6 | Platforms for the net bars? | Linux gate on virtual time; per release a two-host real-process soak (Linux and the Windows rig), one Android session on the test phone over real Wi-Fi, the owner's two-account Steam session; macOS testing only |
| Q7 | Attest DO27's class-P2 files? | Not needed: re-derive; ask only if a lane wants a specific P2 file |
| Q8 | Lockstep host fairness? | Engine default equal delay for the player host (`host_input = Delayed`); each game chooses |
