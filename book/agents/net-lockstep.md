# Net lockstep (agents)

<!-- index: Lockstep -->
<!-- index: Rollback -->
<!-- index: Desync -->
<!-- index: Host migration -->

`regia_net::lockstep` is the engine's lockstep mode (spec 120 § 7.1-7.5, ADR-8146 and ADR-8212): one host decides one frame
per tick, every seat sends its inputs stamped for a later tick and steps its simulation only from frames. This page is
what an agent reads to check a lockstep match: the `net.lockstep` method, what its numbers mean, and how to verify the
behaviors. The player-facing description is in [Networking](../user-guide/networking.md). The task
procedure is [Lockstep and rollback](../how-to/online/lockstep-and-rollback.md).

## `net.lockstep`

`net.lockstep {endpoint?}` reports every lockstep host and seat the game publishes into `NetAgentState` with
`regia_net::lockstep::net::publish_host(host, label, now_ms, state)` and `publish_seat(seat, label, state)`. Without
`endpoint` it lists all of them; without the bridge resource the list is empty.

```json
{ "endpoint": "host" }
→ { "endpoints": [{ "label": "host", "role": "host", "tick": 7204,
      "delay_ticks": 6, "delay_from": 1321, "delay_target_ticks": 6,
      "delay_raises": 2, "delay_lowers": 1, "calm_windows": 3, "gap_ticks": 7,
      "datagrams_sent": 28816, "datagram_bytes_sent": 801122, "stream_bytes_sent": 0,
      "checkpoints_agreed": 240, "last_agreed_tick": 7200,
      "desyncs": 0, "resyncs": 0, "host_resyncs": 0, "ties": 0, "fp_env_corrections": 0,
      "late_stamp_suspects": [],
      "seats": [{ "seat": 1, "frame_lag_ticks": 4, "frame_lag_max_ticks": 9,
                  "inputs_missing": 12, "inputs_late": 3, "inputs_too_far_ahead": 0,
                  "lateness_p99_us": 61200, "loss_run_p99": 2, "delay_need_ticks": 6,
                  "extra_lead_ticks": 0, "resyncs": 0, "self_heals": 0, "resyncing": false,
                  "stream_frames": 0, "hole_repairs": 1, "lagging": false,
                  "member": true, "silent_ms": 16, "interrupted": false, "catching_up": false,
                  "inputs_late_merged": 3, "inputs_held": 0,
                  "late_stamp_suspect": false, "late_stamp_flags": 0,
                  "input_hold_p10_us": 0, "input_margin_p50_us": 38800, "hold_released": false }],
      "match_end_tick": null, "seats_timed_out": 0, "seats_left": 0, "catchups": 0 }] }
```

Host fields:

| Field | Meaning |
|---|---|
| `tick` | The next tick the host builds. |
| `delay_ticks`, `delay_from` | The input delay in force and the first capture tick it applies to. Every seat's frames carry the same pair. |
| `delay_target_ticks` | What the measurements ask for: `ceil(input lateness p99 / tick) + loss delay` of the seat that needs the most (`DelayPolicy::Shared`) or the median seat (`PerSeat`), inside `min_delay_ticks..=max_delay_ticks`. `null` before the first samples. |
| `delay_raises`, `delay_lowers`, `calm_windows` | A raise is at once and to the target. A lower is one tick after `calm_windows_to_lower` (5) calm windows in a row; `calm_windows` is the count so far. |
| `gap_ticks` | Ticks no capture maps to (the start and each raise): listed empty for every seat, not counted as missing. |
| `datagrams_sent`, `datagram_bytes_sent`, `stream_bytes_sent` | Lockstep payload bytes (transport headers not included). Stream bytes are frames that did not fit a datagram, plus quorum orders. |
| `checkpoints_agreed`, `last_agreed_tick` | Checkpoints (every `hash_every` ticks) whose votes were equal, and the last tick with a reference hash. |
| `desyncs`, `resyncs`, `host_resyncs`, `ties` | Checkpoints whose votes differed; state transfers ordered (seats and host); transfers ordered for the host's own simulation; tallies without a strict majority. |
| `fp_env_corrections` | Times the stepping thread's float environment was not the IEEE default when a frame was handed out (it is put back each time). Not zero means foreign code changes it on the simulation thread. |
| `seats[].frame_lag_ticks` | Frames built that the seat has not acknowledged: about a round trip in ticks. Rising without bound means the seat stopped acknowledging. |
| `seats[].inputs_missing`, `inputs_late`, `inputs_too_far_ahead` | Ticks built without the seat's input (gap ticks excluded); inputs whose first copy arrived after their frame; inputs refused for being at or past the host's input buffer. |
| `seats[].lateness_p99_us`, `loss_run_p99`, `delay_need_ticks` | The seat's input lateness p99 over the last window; the loss delay of its link in ticks (the larger direction): the delay that all but 0.5 % of the direction's datagrams stay within, from how often the link loses and how long its runs are, never above the longest run of the last minute (0 on a link that loses under 0.5 %); and the delay the seat alone needs. |
| `seats[].extra_lead_ticks` | Ticks the seat stamps ahead of the frame delay (`DelayPolicy::PerSeat`). |
| `seats[].resyncs`, `self_heals`, `resyncing` | Counted state transfers; self-check repairs; the seat's votes are ignored until one matches the reference again. |
| `seats[].stream_frames`, `lagging` | Frames handed to the stream for the seat; the seat fell behind the frame log and needs a state transfer. |
| `desync_reports`, `last_desync` | Probe reports so far, and the newest: `{checkpoint, voter, reference, first_diverged_tick, last_agreed_tick, subsystem, subsystems}` (spec 120 § 7.6, ADR-8211). `subsystem` is the first diverged subsystem; `null` when the game hashes with `report_hash` only (no rows), when only the world hash differs, or when the rows were gone. |
| `seats[].hole_repairs` | Times the seat reported a frames datagram that started past the frames it had, and got every unacknowledged frame again. Under `FrameRedundancy::Interleaved` that is every loss run of two or more whose first datagram after it was a shallow one: the run is repaired by whichever arrives first, the next deeper datagram or the answer to the report. |
| `late_stamp_suspects`, `seats[].late_stamp_suspect`, `late_stamp_flags` | The seats whose inputs are held back right now (below); per seat the flag and how many times it was raised. |
| `seats[].input_hold_p10_us` | The hold nine in ten of the seat's inputs reached over the last ten seconds: from the datagram that first acknowledged the frame that opened the capture to the input's first arrival, plus the time its acknowledgements trail the transport's round trip when the game feeds that in (`note_transport_rtt`). `0` for an honest seat on any link; `null` under 240 samples. |
| `seats[].input_margin_p50_us` | The seat's median margin over the same window: how long before its frame was built an input arrived. |
| `seats[].hold_released` | Rollback only: the seat's median input over the last two seconds arrived more than the window after its frame was due, so the host builds due frames without holding them for it (its late inputs are merged late); it is held for again once its median margin is two ticks inside the window (ADR-8855). Always `false` in strict mode. |
| `seats[].member` | The seat is listed in the frames. A seat that is catching up to join is not, until its join frame. |
| `seats[].silent_ms`, `interrupted` | Milliseconds since anything of the seat arrived (since its admission when never heard); `interrupted` after `timeout.interrupt_ms` (1.5 s) of it, until it is heard again. |
| `seats[].catching_up` | The seat took a catch-up and has not reported the live edge yet. |
| `seats[].inputs_late_merged`, `inputs_held` | Late inputs merged into a later frame (so not lost); ticks the missing-input policy supplied the seat's input (`SeatInput::Held`). |
| `match_end_tick` | The last frame of the match (`end_match`), `null` while it runs. |
| `seats_timed_out`, `seats_left`, `catchups` | Seats removed for silence past `timeout.timeout_ms`; seats that left on request; catch-ups sent. |

A seat reports `role: "seat"`, `tick` (the next tick it steps), `delay_ticks`, `delay_from`, `gap_ticks`, its own
`datagrams_sent` and bytes, `fp_env_corrections`, and what redundancy did for it:

| Field | Meaning |
|---|---|
| `datagrams_lost` | Frames datagrams its sequence shows missing. |
| `frames_repaired`, `repair_ticks_max` | Frames it got from a later datagram, and the longest such repair in ticks. |
| `member`, `catching_up`, `catchups`, `match_end_tick` | Whether the seat is listed in the frames, is still stepping a catch-up, how many catch-ups it took, and the last frame of the match. |
| `single_losses`, `single_loss_repair_ticks_max` | Times exactly one datagram was missing while the seat had every frame before it, and the worst stall that cost, in ticks. It is 1 on a healthy match: the next datagram carries the lost frame. A loss the next datagram did not repair is counted when its frame arrives, with the whole stall. |

## Late stamping

An honest seat samples the input of a capture tick when it steps the frame before it and sends it in the datagram
that first acknowledges that frame, so at the host the input's first arrival and that acknowledgement are the same
datagram: a hold of zero, on any link (latency, jitter and asymmetry cancel, and after a loss the next datagram
carries both). A modified client can sample later, as late as the input delay allows, and react to newer state than
everyone else. The host flags a seat (`seats[].late_stamp_suspect`, `late_stamp_suspects`, `HostEvent::LateStamp`)
when, over the last ten seconds and at least 240 inputs, nine in ten of its inputs were held two ticks or more and
its median margin before the deadline is no larger than that hold. The flag clears when the window no longer shows
it. **Nobody is kicked for it**: it is for the game and its operators to act on.

What is not flagged: a seat on a slow or lossy link (a small margin, no hold), and an application that samples one
frame after it stepped (a hold of a tick). What the host cannot see: a client that delays every packet it sends, the
transport's acknowledgements included, is a slow uplink to every measurement; what it gains is bounded by the input
delay minus its real round trip, and it is then the seat with the largest `delay_need_ticks`.

## Host migration

When the host quits, the match continues on the best connected seat (spec 120 § 10.5, ADR-8809;
`regia_net::lockstep::HostMigration`). Each seat has one `HostMigration` next to its `LockstepSeat`. The host shares a
`MigrationRoster` with `LockstepHost::share_roster`: each seat's Noise static key and address, the agreed re-host port, and
a per-match migration secret. It goes over the sealed link. A round-trip matrix follows every 3 s and whenever the seats
change. The game feeds the matrix with `note_transport_rtt` and, optionally, `note_pair_rtt`.

The election runs like this:

1. A seat has not heard the host for `migrate_after_ms` (3,000). Silence counts lockstep payloads and, with
   `LockstepSeat::note_host_heard`, the transport's keepalives.
2. Every seat ranks the survivors the same way: the lowest sum of round trips to the other survivors, ties by the
   lowest seat id. Seats that were silent at the host rank last.
3. The winner listens on the agreed port or one of the next 15 (`MigrationAction::Listen`).
4. The others connect with the winner's static key pinned (`MigrationAction::Connect`) and claim their place. They
   send the frames the winner lacks, then their next tick.
5. The winner waits for every survivor or `hello_wait_ms` (15,000). It then resumes at the highest next tick
   (`MigrationEvent::Promoted`, `take_host`).
6. A winner that does not answer within `next_candidate_after_ms` (3,000) is passed over for the next ranked seat.

Every message carries the match id, the migration epoch and the sender's seat. A keyed BLAKE3 MAC under the migration
secret authenticates it. Resume tokens are keyed by the same secret, so a seat, the old host included, can come back
to the re-hosted match with `LockstepHost::resume` and `catch_up`.

`HostMigration::annotate(&mut status)` adds a `migration` object to `net.lockstep`:

```json
"migration": { "phase": "following", "epoch": 0, "current_host": 0, "candidate": 2,
               "ranking": [2, 1, 3], "migrations": 0, "last_resume_tick": null,
               "refused": 0, "last_refusal": null, "matrix_version": 41, "roster": true }
```

| Field | Meaning |
|---|---|
| `phase` | `watching` (the host is live), `following` (electing, connected or connecting to `candidate`), `leading` (this seat won and waits for hellos). |
| `epoch` | Migrations this seat has been through. Messages of another epoch are refused. |
| `current_host`, `candidate` | The seat whose process hosts (`null`: a dedicated host), and the seat this one follows or is while electing. |
| `ranking` | The running election's order, best connected first. |
| `migrations`, `last_resume_tick` | Completed migrations, and the tick the last one resumed at (the highest next tick of the survivors). |
| `refused`, `last_refusal` | Migration messages refused, and the last refusal's code (below). |
| `matrix_version`, `roster` | The newest round-trip matrix this seat holds, and whether a roster arrived (no roster: no migration). |

Readout for the game: `MigrationEvent::HostLost`, `Electing { winner, rank }`, `Resuming { tick, in_ticks }`. The
countdown is `resume_lead_ms`, 1,000 ms by default. `NoCandidate` means every survivor was tried, or a winner got no
hello at all; a seat never resumes the match alone while others may still be in it.

How to verify (the tests are in `crates/regia_net/src/lockstep/migration_tests.rs`, seeded virtual time):

- `the_host_drops_at_several_ticks_with_3_and_8_peers_and_the_match_continues`: the best connected seat hosts. Every
  survivor resumes at the highest next tick and keeps stepping, and all simulation hashes stay equal.
- `the_winner_dies_too_and_the_next_ranked_takes_over_after_3_s`.
- `peers_at_unequal_ticks_resume_at_the_highest_and_catch_ups_pass_the_stream_cap`: the winner lacks 360 frames
  (hello parts) and another seat lacks 450 (stream batches).
- `the_agreed_port_is_taken_and_the_winner_listens_on_the_next_one`, and
  `an_impostor_on_the_next_port_is_skipped_by_the_pinned_key`.
- `the_old_host_rejoins_the_migrated_match_as_a_player`, `the_migration_completes_under_reorder_and_loss`,
  `two_migrations_in_a_row`, `a_non_member_can_neither_win_nor_join_the_election`.

## Error codes

| Code | When |
|---|---|
| `lockstep_unknown_endpoint` | `net.lockstep` named a label no host or seat published. |

`LockstepError::code()` gives the codes a game reports for refused lockstep payloads and calls:
`lockstep_malformed`, `lockstep_input_invalid`, `lockstep_input_too_large`, `lockstep_unknown_seat`,
`lockstep_not_started`, `lockstep_no_local_seat`, `lockstep_capture_closed`, `lockstep_catchup_gap` (a catch-up from a
state older than the frame log, or from a tick not built yet: take a newer state), `lockstep_resume_refused` (the token
names no seat that may come back: forged, the seat left, or past `resume_window_ms`), `lockstep_match_ended` (an input
after the last frame). A match that ends on a tie it cannot decide ends with `desync_unresolved`. `HostEvent::SeatRemoved`
carries the reason: `desync` (after `max_resyncs`), `timeout` (silent past `timeout.timeout_ms`, or past the start
grace for a seat never heard), `left` (the seat asked). A game whose host simulation is outvoted past `max_resyncs`
(`HostEvent::HostFault`) ends the match with `host_desync` until host migration lands
(`regia_net::desync::{DESYNC, HOST_DESYNC, DESYNC_UNRESOLVED}`).

`MigrationError::code()` gives the codes of refused migration messages (`migration.last_refusal`):
`migration_malformed` (does not decode), `migration_no_roster` (the host never shared a roster), `migration_bad_mac`
(another match, or a MAC that does not verify under the migration secret: a process outside the match),
`migration_not_member` (the sender is no seat of the roster or the election), `migration_stale_epoch` (a message of
another migration), and `migration_unexpected` (it does not fit what the seat is doing, for example a hello to a seat
that does not lead).

## Checks

```text
wait_until net.lockstep: endpoints[0].tick > 600
expect net.lockstep {"endpoint":"host"}: endpoints[0].desyncs == 0, endpoints[0].resyncs == 0
expect net.lockstep {"endpoint":"host"}: endpoints[0].delay_ticks <= 15
expect net.lockstep {"endpoint":"seat1"}: endpoints[0].single_loss_repair_ticks_max <= 2
expect net.lockstep {"endpoint":"host"}: endpoints[0].late_stamp_suspects == []
```

- **A lost datagram does not stall.** Put loss on the host with `net.conditioner_set {endpoint:"server", config:{loss:0.03}}`,
  run, and read each seat: `datagrams_lost` rises, `single_loss_repair_ticks_max` stays 1, `stream_frames` stays 0.
  A loss run of two or more is repaired by the first or the second datagram after it; when the first one is a
  shallow one the seat also reports a hole (`hole_repairs` on the host). A burst longer than the redundancy window
  shows as a larger `repair_ticks_max` on the seat (the burst plus a round trip); the window then grows to cover
  such bursts, up to 16 frames.
- **Late stamping.** `late_stamp_suspects` is empty and every `input_hold_p10_us` is `0` on honest clients, whatever
  the conditioner does to the link. A seat listed there holds its inputs back: read its `input_hold_p10_us` and
  `input_margin_p50_us`, and decide by hand.
- **Delay raise and lower.** Add latency (`config: {delay_ms: 60}`): `delay_raises` goes up at once and `delay_ticks`
  reaches the target. Remove it: `calm_windows` counts to 5, then `delay_ticks` drops by one, every 10 s. Every seat
  reports the same `delay_ticks` and `delay_from`.
- **Hash authority.** `desyncs` counts checkpoints with differing votes. A seat with `resyncs > 0` was outvoted;
  `host_resyncs > 0` means the host's own simulation was outvoted and resynced from a seat (no seat is removed for
  it); `ties > 0` means a tally had no strict majority (two seats without an observer). The game sees the verdict as
  `HostEvent::Desync` with the seat, the checkpoint, the last agreed checkpoint and, after the probe,
  `HostEvent::DesyncLocated` with the first diverged tick.
- **The probe names the subsystem.** In a test build of a game that hashes with `record_state`, flip one subsystem on
  one seat at a tick with the game's `DesyncInject` hook, then:
  `wait_until net.lockstep {"endpoint":"host"}: endpoints[0].desync_reports >= 1` and
  `expect net.lockstep {"endpoint":"host"}: endpoints[0].last_desync.subsystem == "economy", endpoints[0].last_desync.first_diverged_tick == 100, endpoints[0].last_desync.last_agreed_tick == 99`.
  The verdict comes at the next checkpoint (`desyncs` is 1 at tick 120 for an injection at 100), the seat's
  `resyncs` is 1, and `resyncing` returns to `false` once its vote matches again.
- **A forged hash never ends the match.** A seat whose votes are forged shows `resyncs` 1, then 2, then leaves the
  `seats` list (removed with `desync`); the other seats' `tick` keeps rising and `last_agreed_tick` keeps moving.

- **Sessions** (ADR-8212). A seat whose game froze shows `silent_ms` rising and `interrupted: true` after 1.5 s while
  the host's `tick` keeps rising at the tick rate (nothing waits for it); with `missing_input: "hold"` its
  `inputs_held` rises instead of `inputs_missing`. It is removed when `silent_ms` passes `timeout.timeout_ms`
  (`seats_timed_out` + 1), never earlier and never for loss. After a reconnect the seat reports `catching_up: true`,
  then `false` within seconds, with the same `seat` number; a new seat appears in `seats[]` with `member: false` until
  its join frame.

```text
expect net.lockstep {"endpoint":"host"}: endpoints[0].seats_timed_out == 0
wait_until net.lockstep {"endpoint":"seat2"}: endpoints[0].catching_up == false
```

## Tests and the soak

- `cargo test -p regia_net --lib lockstep`: the core on virtual-time links (redundancy, delay switches, host input,
  stream fallback, hostile bytes, the quorum rules). `lockstep::tests::sessions` holds the DO27 session scenarios: a
  silent seat never blocks the frame, late inputs merged with no press lost or doubled, a clean leave and the match
  end on the same tick everywhere, a killed seat removed at the timeout while the match ticks on, 3 s and 8 s stalls
  kept and 12 s past a 10 s timeout ejected, a slow loader kept inside the grace, a reconnect after 15 s in step within
  5 s with the same seat id, a late join, and the catch-up gap and budget.
- `cargo test -p regia_net_soak --test lockstep`: a host on a real `NetServer` and seats on real `NetClient`s over the
  soak kit's impaired network: 4 seats on `bad-wifi` for 5 minutes, `lockstep-mix` at 8, 16 and 40 seats, and the host
  upload bar on two input mixes over 5 minutes: 40 seats on the order-driven mix and 16, 24 and 40 seats on the
  action mix (the bar is stated through 24 seats there). `cargo xtask net-soak` runs the same scenarios as the
  `lockstep-mix`, `lockstep-mix+action` and `lockstep-mix+bots` profiles ([Net soak](net-soak.md)).
- `cargo test -p regia_net_soak --test lockstep_sessions -- --nocapture`: the session half over the real transport:
  4 seats on the `flappy` link (a 3 s outage every 20 s for every seat) for 70 s, with the fourth seat's long
  outage pinned at 12 s. The named profile's long outage is the removal timeout plus 2 s and ends the connection
  (ADR-8209); 12 s is past the frame log and inside the timeout. The short outages keep the seat and its frames;
  the long one passes the frame log, the host answers `SeatLagging` with a catch-up of its serialized simulation,
  and the seat returns with its seat id. Every seat ends in step, nobody removed, no desync.
- `cargo test -p regia_net_soak --features desync-inject --test lockstep --test lockstep_procs`: forced divergence. A
  seat's (or the host's) simulation is perturbed through the test-only fault hook, in process on the impaired network
  and with one real server process and 2, 3 and 4 real seat processes on loopback UDP (`lockstep_bot`); the detector
  must name the seat, the checkpoint and the first diverged tick. The hook is in a build exactly when the
  `desync-inject` feature is named (`regia_net_soak::DESYNC_INJECT`); nothing else enables it.
- `cargo xtask net-soak --profiles lockstep-mix --clients 40 --seconds 300` (add `--light` to skip the heap figure):
  the same match through the soak's plan, with `lockstep_upload` and `desync_zero` in `summary.md`
  ([Net soak](net-soak.md#lockstep-runs)); `lockstep-mix+action` is the action input mix and `lockstep-mix+bots` is the wander-fire bot mix (40 seats, 600 s in the gate; ADR-8409). A two-machine UDP recipe is `cargo xtask net-soak --two-host HOST_A HOST_B` ([Two hosts](net-soak.md#two-hosts)).
- `cargo test -p regia_net_soak --test lockstep_golden`: the cross-architecture pin. Run it on every platform; the
  digest must be the same number everywhere.

### Per-system vectors and DO27's steering golden (ADR-8307)

`regia_net_soak::vectors::drive_vectors` steps a game's per-system hash vector (one `u64` per system and tick)
through `LockstepHost` and `LockstepSeat` in memory: implement `VectorWorld` (`Clone` is the snapshot; `step` returns
the tick's words, or `None` for a frame that stepped nothing), give one input closure and one world per seat, and read
`VectorRun`: each seat's rows (`in_step(ticks)`), the verdicts, the host's first-diverged frames and the per-system
probes (`VectorProbe`: row and system of the first difference, taken when a transfer was ordered).
`VectorDrive::fixed_delay(seats, ticks)` keeps the input delay at 4 ticks, so capture `k` is stepped at frame `4 + k`.
`VectorText::parse` / `encode` read and write the golden text form (`# header`, then `<row from 1> <16 hex digits>
...`); error codes `vector_no_header`, `vector_bad_row_number`, `vector_bad_word`, `vector_ragged`. `first_divergence`
names the first row and system two runs differ at: run it against a golden to bisect a mismatch.

DO27's regia-absorb-3 steering golden (`tests/fixtures/do27/steering-1200.txt`, SHA-256
`535ce827795375e623e64317e41ddfa118a5d0eb33c956d1b001c52fa235dbb5`) is replayed through it:

- `do27_steering_golden_words_through_the_lockstep_driver_keep_their_sha`: 8 seats, the two driven hulls' stick script
  as seats 0 and 1's inputs, each frame's inputs checked against the script, the recorded words returned; the rows must
  match the golden and hash to its SHA-256 on every platform.
- `a_drifted_system_word_is_named_with_its_tick_and_repaired_through_the_driver`: seat 3's `ai` word drifts at tick
  777; the verdict at checkpoint 810 names seat 3, the probe locates frame 781 and row 777 / `ai`, and the transfer
  brings the seat back to the golden.
- `do27_steering_golden_through_the_lockstep_driver_matches_its_sha` stays ignored: the words fold DO27's whole game
  state, so only DO27's simulation can recompute them. DO27 runs that check in its repository with its own
  `VectorWorld`.
- `cargo xtask det-scan`: the lockstep module and the sample simulation are on the lockstep closure
  (`docs/ci/lockstep-closure.toml`, `[[module]]` rows).

## Rollback lockstep

Under `LockstepConfig::mode = LockstepMode::Rollback { max_rollback_ticks }` (spec 120 § 7.7, ADR-8361;
`LockstepConfig::rollback(60)` is the preset: delay 1-3 ticks, window 8, presses cleared for a missing input) a seat
does not wait for frames. A `regia_net::lockstep::RollbackRunner` steps the seat's game state on the seat's own
clock with predicted inputs (its own input as sampled; every other seat's last input with its presses cleared; every
seat absent on a gap tick), at most the window past the newest frame, and replays from the first tick whose real
inputs differ when the frame arrives. Only confirmed ticks are hashed and voted on, so the quorum (§ 7.6) sees the
strict match's states. The host holds a frame for a live seat's input for up to twice the window: an input is waited
for, not dropped. A seat whose median input arrives more than the window late (its round trip is longer than the delay
plus the window plus its clock lead cover) is not held for: holding could not bring most of its inputs in and would
make every other seat wait at its window's edge. That seat's inputs are merged late, so it alone pays for its link
(`seats[].hold_released` on the host's row; ADR-8855, KI-NT-038). `LockstepConfig::release_slow_seat_hold = false`
keeps the pure hold: every seat waits for the slowest.

A seat's `net.lockstep` row adds (a host reports `rollback_window_ticks` only):

| Field | Meaning |
|---|---|
| `rollback_window_ticks` | The window, ticks; `0` in strict mode. |
| `rollbacks`, `rollback_ticks_max`, `rollback_ticks_replayed` | Frames whose real inputs differed from the prediction (each replayed from the first such tick), the deepest replay (never more than the window), and the ticks replayed in total. |
| `ticks_predicted`, `predictions_confirmed` | Ticks stepped ahead of the frames, and predicted ticks a frame confirmed without a replay. |
| `window_stalls`, `ahead_ticks` | Application frames in which the seat's clock was past the window and it waited for a frame; how far ahead of its newest frame it is now. |
| `local_input_lag_ticks` | `[fewest, most]` ticks between sampling a local input and stepping it: the input delay the player feels. With a fixed delay both are the delay. |
| `clock_lead_ticks` | How far the seat's clock runs ahead of its first frame's arrival (negative: behind), as the clock sync steered it so its median input reaches the host on time; at most the window either way. |

```text
expect net.lockstep {"endpoint":"seat0"}: endpoints[0].rollback_ticks_max <= 8
expect net.lockstep {"endpoint":"seat0"}: endpoints[0].local_input_lag_ticks[1] <= 3
expect net.lockstep {"endpoint":"host"}: endpoints[0].desyncs == 0
```

A rising `window_stalls` on one seat means its link's round trip is longer than the delay plus the window cover: raise
the delay (`max_delay_ticks`) or play strict lockstep. The other seats do not wait for it once the host has released
the hold for it. `a_seat_whose_inputs_miss_the_window_is_not_held_for_and_the_other_seat_runs_free`: a 450 ms uplink;
in the last six seconds the other seat waits at its window on under 1 % of its frames and no frame is built more than
two ticks late. The tests: `cargo test -p regia_net --lib lockstep::tests::rollback` (a
fighting-style sample on the virtual-time rig, its confirmed state against a lockstep reference) and
`cargo test -p regia_net_soak --test lockstep_rollback` (two seats on `transatlantic` at delays 1, 2 and 3 against the
reference stepped offline from the input scripts; four seats on `bad-wifi` for ten minutes without a desync).
