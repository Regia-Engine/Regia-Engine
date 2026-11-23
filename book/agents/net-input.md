# Net input (agents)

How a `regia_net` client's per-tick input reaches the server's tick driver, what limits apply to large inputs, and
how to read it in `net.status` (spec 70 § 6.9, ADR-7896). There is no separate method: the counters are in the
`totals` of each endpoint that [`net.status`](net.md) reports.

## The two lanes

`NetClient::send_input(tick, input)` picks the lane by size:

| Input (encoded) | Lane | Redundancy | On loss |
|---|---|---|---|
| Whole remembered history fits one datagram | unreliable, one bundle | every remembered input (`input_redundancy`, default 8) | the next send repeats it |
| Larger inputs, each one fits a datagram | unreliable, up to 4 bundles per send, newest first | as many as fit; a 548-byte input has 8 ticks of cover per send | a later send repeats it |
| One input larger than a datagram, up to 16 KiB | reliable, fragmented by the channel | resent until acknowledged | arrives one resend timeout later, intact, on its tick |
| Over 16 KiB | refused: `SendError::TooLarge`, nothing changes | | |

One datagram holds 1153 bytes of bundle on a sealed link at the default 1200-byte packet cap (`input_bundle_budget`).
A refused send leaves the client's input history as it was; the next `send_input` works as usual.

## Reading it

```json
{}
→ { "bridge": true, "endpoints": [{ "label": "client", "role": "client", "state": "connected",
    "totals": { "datagrams_out": 3120, "malformed": 0, "input_bundles_trimmed": 412, "reliable_inputs": 3, ... } },
    { "label": "server", "role": "server", "totals": { "malformed": 0, "reliable_inputs": 3, ... } }] }
```

- `input_bundles_trimmed` (client): sends whose first datagram could not hold the whole history. Growing steadily means
  the inputs are large; each input still goes out several times, but a burst of loss longer than the cover drops ticks
  (the server's `missed_input_ticks` in `net.peers` shows it).
- `reliable_inputs`: on a client, inputs sent on the reliable lane; on a server, the ones it accepted. A server count
  below the clients' sum means inputs were refused or arrived after their tick was simulated.
- `malformed` (server): a refused reliable input. The server raises `ServerEvent::Rejected` for that peer (ADR-7892):
  `input_too_long` for an input over 16 KiB (the peer is disconnected with `protocol_error`), `input_malformed` for
  one that does not decode as the game's input type (one malformed point), `unknown_lane` for the reliable input lane
  on the unreliable channel. A correct client sends none of them.

## Input timing: feedback, clock and lead (ADR-8077)

The server tells each player how its inputs arrive (spec 120 § 6.4), and the client stamps its inputs from that
(§ 9.1, § 9.2). A client cannot see that its inputs are late when only the downstream queues (a bulk download on the
same home link): its tick syncs arrive late, so its estimate of the server tick lags. Only the server can say so.

- **Feedback.** With every tick sync (250 ms) and at once (at most once per tick) after a late or missing input, the
  server sends a `LinkFeedback`: newest contiguous input tick, arrival margin p50 and p5 over the last 2 s (the input's
  tick minus the server's next tick when its first copy arrived; negative means late) and the input tick that set the
  p5 (`margin_p5_tick`), `inputs_late`,
  `inputs_missing`, `inputs_too_far_ahead`, the upstream loss it measured, and its `input_buffer_ticks`. Players that
  have sent no input yet, and spectators, get none.
- **Clock.** `estimated_server_tick` comes from the least delayed of the last 8 tick syncs plus half the round trip. It
  never moves backward: it runs up to 5 % fast or slow toward a new offset and steps forward only when more than 8
  ticks behind (`clock_resyncs`).
  Until the connection has measured a round trip of its own, the round trip is the handshake's: the shortest of its
  legs, each counted from a flight's first send to the last `receive` that did not hold the answer, less one tick for
  the server's wait (ADR-10096). So the estimate is on the server's tick from the accept on, on any path; before, it
  was the whole one-way delay behind until the first round trip came back (45 ticks for 3 s at 1,500 ms one way).
- **Lead.** `input_lead_ticks()` aims for a p5 margin of at least one tick: it rises at once by the deficit, falls by
  one tick after 5 calm seconds and stays two ticks inside the server's input buffer. Before the server's first
  feedback it is two ticks, two measured jitters and the path's one-way delay as the handshake measured it (half its
  shortest round trip less the server's tick, ADR-10096), so the first inputs of a slow path are stamped for ticks they can still reach: the server has them in
  time one one-way delay after the connect (1.56 s at 1,500 ms one way; it was 4.73 s, and the lead then sat at the
  buffer's cap of 62 ticks where 48 are needed). `due_input_ticks(now)` returns
  the ticks to send inputs for, so a raised lead or a clock step leaves no gap. When the p5 margin goes below zero the
  client raises `ClientEvent::InputsLate { margin_p5 }` once per late spell.
- **Redundancy (ADR-8312).** Each input packet repeats the newest inputs the server has not acknowledged
  (`newest_contiguous_tick`), up to `input_redundancy` (default 32, a whole bundle). A measured loss that needs more
  copies for 0.1 % residual loss raises a lower setting (`input_copies`: `ceil(log 0.001 / log loss)`, 3 at 5 % loss);
  an input that needs its own bundle is repeated in older bundles until it rode in that many datagrams (at least 2). A
  skipped or repeated tick keeps the history (a repeated tick keeps its first input). A game codec may code each input
  against the one before it in the bundle (`NetCodec::encode_delta` / `decode_delta`; the default writes the full
  value).

`net.status` reports it for a client under `input_timing`, and `net.peers` under each player's `input_feedback`:

```json
{}
→ { "endpoints": [{ "label": "client", "role": "client", "state": "connected", "tick": 18311,
    "input_timing": { "lead_ticks": 6, "clock_resyncs": 0, "late_spells": 1, "unacked_inputs": 14, "input_copies": 2,
      "feedback": { "server_tick": 18305, "newest_contiguous_tick": 18310, "margin_p50": 5, "margin_p5": 3,
        "margin_p5_tick": 18296, "margin_samples": 60, "inputs_late": 41, "inputs_missing": 44, "inputs_too_far_ahead": 0,
        "upstream_loss": 0.01, "input_buffer_ticks": 64 } }, ... }] }
```

Read it this way: `margin_p5` at or above 1 means inputs arrive with a tick to spare; `inputs_missing` growing while
`margin_p5` is fine means loss bursts longer than the lead (inputs are repeated until acknowledged, so only a burst
that outlasts the lead loses them; `unacked_inputs` near 64 says the feedback is not getting through); a large
`lead_ticks` with `upstream_loss` near 0 means the downstream path is queued (bufferbloat on the player's link), not the
uplink; `inputs_too_far_ahead` growing means a client stamping past the server's buffer. To see it react, apply a
conditioner with delay on the server side only (`net.conditioner_set` on the server's label with
`{"config": {"enabled": true, "delay_ms": 300}}`): the client's `late_spells` counts one, `lead_ticks` rises and
`inputs_missing` stops growing within about a second.

## Example: a large order

A game sends a 4 KiB order on tick 410 between small inputs. Step the game with `play.step`, then read `net.status`:
the client's `reliable_inputs` is 1, and once the server simulated tick 410 its `reliable_inputs` is 1 as well and
`net.peers` shows no new `missed_input_ticks` for that player. Under a lossy conditioner (`net.conditioner_set` with
`{"profile": "wan", "config": {"loss": 0.1}}`) the order may arrive a resend later but still on tick 410, as long as
the client stamped it far enough ahead of the server's tick.

## Limits

- `MAX_RELIABLE_INPUT_BYTES` = 16 KiB per input; `MAX_INPUT_BUNDLES_PER_SEND` = 4 datagrams per send; at most 32
  inputs in one bundle.
- The server buffers at most `input_buffer_ticks` (64) future ticks per peer, so reliable inputs hold at most 1 MiB per
  peer. Inputs for ticks already simulated, or beyond the buffer, are dropped.
- A reliable input waits behind earlier reliable messages and holds later ones while a lost fragment is resent.
- Wire version 4 (input feedback, ADR-8077): a client or server built before it is refused at the handshake with
  `protocol_mismatch`.

## Errors

No method of its own, so no error codes. A malformed feedback record (loss over 100 %, an input buffer of zero ticks)
is counted in the client's `totals.malformed` and ignored. `send_input` returns `SendError::TooLarge { len, max }` for an input over
16 KiB, `SendError::Backpressure` when the reliable window is full, and `SendError::NotConnected` before the accept.
