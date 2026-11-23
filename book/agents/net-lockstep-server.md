# Net lockstep server (agents)

A dedicated server can be the frame authority of a lockstep match instead of running the project's own game
(ADR-8558). `server.toml`'s `[lockstep]` section turns it on; the game hands the host a `LockstepHostGame`
(`regia_net::server_host::lockstep_game`) built on the ECS-free `regia_net::lockstep::server::LockstepServer`. Up to
`max_seats` seats (default 40) send inputs; the server builds one frame per tick on its own clock. With `observer = true`
the server also steps the game's simulation on its own thread and votes its state hash in the quorum
([Net lockstep](net-lockstep.md)), as one member that never sends inputs: with two seats, the observer makes a planted
desync a 2-of-3 verdict that names the diverged seat.

**The seats never wait for the observer.** An observer more than `observer_max_lag_ticks` behind leaves the quorum
(checkpoints close on the seats' votes), keeps stepping, and votes again once it is back within half that lag. More than
`observer_max_backlog_ticks` behind it is detached for the match. Every step of that is counted.

**An outvoted observer takes the majority's state** (ADR-8855). When the observer's hash is the minority at a checkpoint,
the host orders a state transfer (`HostEvent::ResyncHost`, naming the majority seat and the checkpoint) and that seat gets
`SeatEvent::ProvideState`. If the game's observer simulation can load a state (`LockstepObserver::loads_state` and
`load_state`), the game hands the seat's bytes to `LockstepServer::load_observer_state` (a `LockstepHostGame` asks its
`LockstepSeatLink::observer_state` each step while `observer_transfer()` names a tick). The observer thread loads the
state, steps the frames after the checkpoint, and votes again on it; once that hash is the reference the observer counts
again (`observer.resyncs`). While the transfer is pending its votes are not counted, and the seats never wait for it. A
simulation that cannot load a state, or refuses the bytes, leaves the quorum for the match (`observer.diverged`), as
every observer did before v0.7.

**Delay grief.** Under the default `DelayPolicy::Shared` the frame delay follows the seat that needs the most, so only
what the host measures sizes it: each seat's input lateness and the loss runs of its inputs datagrams. A downlink loss
run or an extra lead a seat reports does not raise every seat's delay (ADR-8656); under `PerSeat` it counts in that seat's
own need, and the frame delay follows the lower median, which one seat cannot move. `net.lockstep`'s per-seat
`loss_run_p99` still shows the larger of the measured and the reported run; `delay_need_ticks` is the need the policy
uses.

## `server.toml`

```toml
[lockstep]
enabled = true
max_seats = 40                    # 1..=4096
input_delay_ticks = 4             # 1..=1000
observer = true
checkpoint_ticks = 30             # 0..=3600; 0 = no hash votes
observer_max_lag_ticks = 60       # 1..=3600
observer_max_backlog_ticks = 600  # observer_max_lag_ticks..=65536
```

The full key table is in [`docs/formats/server-toml.md`](../../docs/formats/server-toml.md). A bad value is refused with its
field (`lockstep.max_seats: 1..=4096`) and the host exits with code 2; the section needs a restart (`server.reload_config`
lists `lockstep` under `needs_restart`).

## `server.lockstep_status`

Read-only. The lockstep match's rows: the same object as the `lockstep` field of `/health` and `server.status`.

```json
{ "method": "server.lockstep_status" }
→ { "tick": 7204, "seats": 40, "max_seats": 40, "frames_built": 7204, "seats_refused": 1,
    "delay_ticks": 4, "checkpoints_agreed": 238, "last_agreed_tick": 7170,
    "desyncs": 1, "resyncs": 1, "verdicts": 1,
    "last_verdict": { "tick": 120, "kind": "diverged", "agreeing": ["observer", "seat0"],
                      "diverged": ["seat1"], "abstained": [] },
    "observer": { "voting": true, "attached": true, "next_tick": 7203, "lag_ticks": 1, "lag_max_ticks": 74,
                  "frames_stepped": 7203, "dropouts": 1, "rejoins": 1, "detached": false, "diverged": false,
                  "resyncs": 0, "transfer_tick": null, "self_checks": 0, "step_us_p50": 210, "step_us_p99": 940, "step_us_max": 3100, "stalls": 0 } }
```

| Field | Meaning |
|---|---|
| `tick` | the next tick the server builds |
| `seats`, `max_seats`, `seats_refused` | seats in the match, the cap, seats refused because the match was full |
| `delay_ticks`, `checkpoints_agreed`, `last_agreed_tick`, `desyncs`, `resyncs` | the host's rows of `net.lockstep` |
| `verdicts`, `last_verdict` | desync verdicts so far and the newest: its checkpoint, `kind` (`self_healed`, `diverged`, `split`, `unresolved`) and the voters on each side (`observer`, `seat<N>`) |
| `observer` | `null` without an observer. `voting`: its hash counts now; `lag_ticks`, `lag_max_ticks`: how far it trails the newest frame; `dropouts`, `rejoins`: times it left and came back into the quorum; `detached`: it fell past the backlog cap and gets no more frames; `diverged`: the quorum outvoted it and it could not take the majority's state, so it votes no more this match; `resyncs`: state transfers it took after being outvoted; `transfer_tick`: the checkpoint of the transfer it waits for (`null`: none); `step_us_*`, `stalls`: its measured step times (the last minute of steps; stalls are steps over 100 ms) |

The console command `lockstep` prints the same rows as two or three lines.

### Errors

Every error is `F0002` with `data.reason`:

| Reason | When |
|---|---|
| `server_host_missing` | The process is not a dedicated-server host. |
| `lockstep_not_running` | The host's game is not a lockstep server (`[lockstep] enabled = false`, or the game is not a `LockstepHostGame`). |

The host-side errors of `LockstepServerError::code` are not agent errors (the server logs them and counts them):
`lockstep_seats_full` (a seat past `max_seats`), `lockstep_observer_missing` (`observer = true` and the game gave no
simulation), `lockstep_observer_thread` (the observer thread did not start), `lockstep_settings_invalid`,
`lockstep_observer_no_transfer` (`load_observer_state` for a tick no transfer is pending for: none was ordered, a newer
one replaced it, or it was taken), `lockstep_observer_state_too_old` (the host no longer holds the frames after the
transfer's tick; the transfer is ordered again at its deadline).

A seat's payload that does not decode (`lockstep_malformed`, `lockstep_input_invalid`, `lockstep_input_too_large`) came
over the seat's own connection, so the host scores it as the transport scores a malformation (ADR-8734): 16 points, a
leak of 4 points a second, removed at 1,024. One bad payload is never fatal; a seat that keeps sending them leaves the
match in about a second (`LockstepHostGame::seats_removed`), and the link's `LockstepSeatLink::refuse` is called so the
transport drops its connection. A game that runs `lockstep::net::host_receive` itself does the same with
`lockstep::net::SeatScores`.

## Verifying

- `cargo test -p regia_net_soak --no-default-features --test lockstep_server_host`: on the ECS-free core (the test
  asserts `regia_net::ECS_FEATURE` is false), 40 in-process seats on the soak kit's `LockSim` play 600 ticks of the test
  clock with the observer voting (41 voters, no verdict; the 41st seat is refused); two seats and the observer give a
  planted desync on seat 1 a `diverged` verdict naming `seat1` with `observer` and `seat0` agreeing; an observer held by
  `ObserverThrottle` for 300 ticks changes nothing the seats see pass for pass (same ticks, frames and states as an
  unthrottled match), leaves the quorum once and rejoins once after its release; a stalled observer past its backlog is
  detached. `an_outvoted_observer_takes_the_majority_state_and_votes_again` (ADR-8855): the observer drifts after
  tick 100, the two seats outvote it, it takes seat 0's state, and when seat 1 drifts after tick 400 the observer and
  seat 0 outvote it 2 of 3; `an_observer_that_cannot_load_or_refuses_the_state_leaves_the_quorum`: the fallbacks and
  `lockstep_observer_no_transfer`.
- `cargo test -p regia_net_soak --no-default-features --test lockstep_server_endpoints` (KI-NT-040, ADR-8855): the same
  server behind a real `NetServer`, 40 real `NetClient`s on the impaired network (handshake, encryption, congestion
  control, the reliable channel, path validation), 40 s each on `bad-wifi`, `mobile-4g` (NAT rebindings), `lockstep-mix`,
  and `hostile` links with an off-path attacker sending garbage from its own address and with seat 0's address forged.
  Every run: every seat connects and stays connected, the seats step every frame in order and share one hash, no
  verdict, the observer voting, frames on the clock from the start, nothing refused either way, no seat marked lagging,
  seat provenance, and a seat whose game goes quiet (its connection stays up) removed by `TimeoutPolicy`, never early.
- `cargo test -p regia_net_soak --no-default-features --test lockstep_server_soak` (ADR-8656): 40 seats and the observer
  on the soak kit's impaired links, 40 s of virtual time each: `bad-wifi`, `mobile-4g` (with its NAT rebinding),
  `lockstep-mix`, and `hostile` (one seat replays seat 0's datagrams as its own, floods duplicates, and sends truncated,
  garbage and garbage-stream payloads). Every run: the seats that stayed step every frame in order and share one hash at
  their common tick, no verdict, frames on the clock, a silent seat removed by `TimeoutPolicy` (never before
  `timeout_ms`) with frames going on, no honest payload refused, and every input a frame lists under a seat is one that
  seat sent. On `hostile` the replayed inputs land on the replayer only and the delay stays at the initial 4 ticks.
- `cargo test -p regia_net --lib lockstep::tests::a_reported`: a reported downlink run or lead does not move the shared
  delay; `per_seat_delay_follows_the_median_not_one_reporting_seat`.
- `cargo test -p regia_net --lib server_host::lockstep_game`: the server host runs the lockstep game from `server.toml`
  on its test clock, refuses the seat past `max_seats`, and shows the rows in `/health` and the console.
- `cargo test -p regia_net --lib server_host::config`: `server_toml_lockstep_section_round_trips`,
  `a_bad_lockstep_field_is_named`.

The observer's step times are measured wall-clock values for the status; no test asserts on them.
