# Networking

`regia_net` is client-server networking over UDP with an authoritative server tick (ADR-1600). This page covers
connections, sessions, input collection and the tools around them, and below, entity replication, client
prediction and interpolation, which the crate provides (mark entities `Replicated` and let the server replicate them
rather than sending your own state messages).

## Modes, transports and finding a game

- **Modes.** `NetMode` is `Offline` (the default), `ListenServer` (hosts and has local players), `DedicatedServer`
  (hosts with no local players, no rendering, no audio) or `Client`. A dedicated server is a separate `server`
  build of the game, run with `regia server` in development and packaged with `regia package --server`; see
  [Net dedicated server (agents)](../agents/net-dedicated-server.md).
- **Sessions.** `NetSessionPlugin` adds the `NetSession` that takes a client from connect through the join handshake,
  level loading and travel to `in_game`, and a server through bans, kicks and reconnects; agents drive it with
  `session.host`, `session.connect` and `session.state`. See [Net session (agents)](../agents/net-session.md).
- **Transports.** Every layer runs over a datagram socket: `UdpTransport` (raw UDP), the in-memory `LoopbackSocket`
  for tests, or `regia_steam::SteamSocket`, which carries the same datagrams (at most 2048 bytes each) over Steam's
  relay network to a Steam id, with NAT traversal and no exposed addresses.
- **Lobbies and matchmaking.** `LobbyPlugin` adds the `Lobbies` resource: a local in-process backend
  (`LocalLobbyHub`) for tests and LAN builds, or Steam lobbies through `regia_steam::SteamLobbyPlugin`. Players
  gather, mark themselves ready, and the owner launches; every member then connects to the published session. See
  [Net lobbies (agents)](../agents/net-lobbies.md).
- **Content handshake.** A joining client must match the server's protocol version (`version_mismatch`), protocol
  hash (`protocol_mismatch`) and content manifest, the gameplay content and mods (`content_mismatch`, with lines
  such as `mod:speedrun: different`). `session.manifest` shows a process's manifest. Engine-created lobbies carry
  the game, protocol hash and content hash, and lobby lists and matchmaking filter on them (content unless
  `lobby.match_content` is false in `config/net.ron`), so only compatible games show up.

## Your game's vocabulary

Everything game-specific is yours. Implement `NetCodec` for your message type, your per-tick input and your join
data, and name them in one `NetGame`:

```rust,wrap
use regia_ecs::prelude::*;
use regia_net::prelude::*;

# type MyMessage = Vec<u8>;
# type MyInput = u32;
# type MyLoadout = ();
struct MyGame;

impl NetGame for MyGame {
    type Message = MyMessage;   // sent on Channel::GAME_RPC or Channel::Unreliable
    type Input = MyInput;       // one per player per tick
    type JoinData = MyLoadout;  // sent with the join request; use () for none
    const GAME_ID: &'static str = "com.example.mygame";
    const PROTOCOL_VERSION: u32 = 3;          // bump on any incompatible message change
    fn content_hash() -> u64 { 0 }   // optional: refuse peers with other data
}
```

`NetCodec::decode` reads bytes from the network: return an error on bad input, never panic. `WireWriter` and
`WireReader` give little-endian fields and length-checked strings and byte arrays.

Peers with a different `GAME_ID`, `PROTOCOL_VERSION` or `content_hash` are refused at the handshake with
`protocol_mismatch`; a server with `ServerConfig::build` set also refuses other build ids (`build_mismatch`).

## Server

```rust,ignore
let socket = UdpTransport::bind("0.0.0.0:7777".parse()?)?;
let mut server: NetServer<MyGame, _> = NetServer::new(socket, ServerConfig {
    max_peers: 8,
    tick_rate_hz: 30,
    password: Some("friday".into()),   // optional; kept only as a salted hash
    ..ServerConfig::default()
});
let mut clock = TickClock::new(30, 4);

loop {
    let now = elapsed_ms();
    server.receive(now);
    while let Some(event) = server.poll_event() {
        match event {
            ServerEvent::PeerJoined { peer, name, role, join } => { /* spawn the player */ }
            ServerEvent::PeerLeft { peer, reason } => { /* remove it */ }
            ServerEvent::Message { peer, channel, message } => { /* chat, commands, ... */ }
            ServerEvent::Rejected { peer, kind, error } => { /* refused bytes; see below */ }
        }
    }
    for _ in 0..clock.advance(frame_ms) {
        let tick = server.driver().tick();
        server.advance_tick(|tick, inputs| world.step(tick, inputs));
        server.broadcast(Channel::Unreliable, &MyMessage::State(world.snapshot(tick)));
    }
    server.flush(now);
}
```

`advance_tick` hands your simulation the inputs that arrived for that tick, in peer-id order. A player whose input
did not arrive is absent from the map; your rules decide how to cover the gap (repeat the last input, stand still).
Each peer's `missed_input_ticks` counts those ticks. Spectators join and receive messages; the tick driver ignores
their input.

**Hostile input (ADR-7892).** The server refuses every message it cannot decode and raises
`ServerEvent::Rejected { peer, kind, error }`; nothing of it reaches your game or the tick driver. An input bundle with
a field past its cap (`WireError::TooLong` from your `NetGame::Input` codec, such as an over-long command tail, or a
bundle of more than 32 inputs) is a protocol violation: the server disconnects that peer at once
(`kind: RejectKind::InputTooLong`, then `PeerLeft` with `ProtocolError`), and no one else. Other refusals
(`InputMalformed`: truncated, an invalid value, bytes left after the bundle; `MessageMalformed`; `UnknownLane`;
`SpectatorInput`, any input from a spectator, which has no seat) score
one malformed point each; the score leaks 4 points a second (no packet earns a point back, ADR-8181), and at 1024 points the
peer leaves with `MalformedPacket`. For a stricter rule, act on the event:

```rust,wrap
# use regia_net::{DatagramSocket, DisconnectReason, NetGame, NetServer, ServerEvent};
# fn handle_events<G: NetGame, S: DatagramSocket>(server: &mut NetServer<G, S>) {
while let Some(event) = server.poll_event() {
    if let ServerEvent::Rejected { peer, kind, error } = &event {
        regia::prelude::warn!("refused {} from {peer}: {error}", kind.as_str());
        if !kind.is_fatal() {
            server.kick(*peer, DisconnectReason::Kicked); // zero tolerance
        }
    }
}
# }
```

**Floods (ADR-8181).** Each connected peer has a packet, byte and message budget per second, checked before the server
spends any decryption on its datagrams. The defaults follow your tick rate and link settings: four times what a client
can send at one `max_packets_per_poll` per tick (7,680 datagrams a second at 30 Hz), 4,096 messages per tick, a burst of
64 datagrams. Over the budget a datagram is dropped unopened (`totals.over_rate`); a peer that stays over it for two
seconds gets `ServerEvent::Rejected { kind: RejectKind::Flood, .. }` and leaves with `DisconnectReason::Flood`. A short
burst (a Wi-Fi radio releasing 200 ms of held packets) is fine. Each receive also serves peers fairly: one peer gets at
most `max(256, 4096 / peers)` datagrams handled per receive, so a flooder cannot push the other players' packets out.
Tune it with `ServerConfig::inbound`:

```rust
use regia_net::{InboundRateConfig, ServerConfig};

fn server_config() -> ServerConfig {
    ServerConfig {
        inbound: InboundRateConfig {
            packets_per_s: 20_000, // 0 = derived from the tick rate
            flood_disconnect_ms: 3_000,
            ..InboundRateConfig::default()
        },
        ..ServerConfig::default()
    }
}
```

`server.peer_inbound(peer)` returns a peer's drops, flood timer and malformed points, and `server.inbound_rates()` the
budget in force.

**Refusals in the replication runtime (ADR-7984).** With `NetPlugin`, the runtime refuses two kinds itself and names
the sender in `NetNode::take_rejections()` (`NetRejection { peer, kind, detail, tick }`): a message type only the server
may send (`ToOwner`, `Broadcast`) arriving from a client is `ServerOnlyMessage` and disconnects that client with
`ProtocolError`; a voice packet that does not parse is `VoiceMalformed`, costs a token of the sender's voice rate and
scores one malformed point (1024 drop the peer with `MalformedPacket`). If your own system decodes bytes a client sent,
charge a bad one with `NetSender::reject_peer(peer, RejectKind::MessageMalformed, "my blob")`. `RejectKind` is
`#[non_exhaustive]`: match it with a wildcard arm.

A host that runs the runtime through `ServerBridge` gets these refusals from `bridge.pump_in(..)` as
`ServerEvent::Rejected { peer, kind, error }` too, with `error` = `WireError::Invalid(detail)`, before the endpoint's own
events of that pump (so a fatal one comes before its `PeerLeft`); read one stream or the other, not both (ADR-10077).
The agent method `net.rejections` counts every refusal per peer and code, transport and runtime, peers that left
included ([Net rejections per peer](../agents/net-rejections.md)).

On the client, a server message that fails to decode on the reliable channel ends the connection with
`ClientEnd::Disconnected(DisconnectReason::ProtocolError)`: the stream is ordered, and skipping a message would leave the
client silently out of step. A bad unreliable message is dropped and counted in `totals().malformed`.

**Readiness for the players you choose.** `server.driver().all_ready_for(tick)` is true when every registered player
has input for `tick`. To step on a set you pick, for example your live players while a player that is leaving cleanly
waits for its removal tick, ask `server.driver().ready_for(tick, live.iter().copied())`, or one peer at a time with
`has_input(peer, tick)`. Neither takes or counts anything, so asking every frame is free of side effects.

`server.kick(peer, DisconnectReason::Kicked)` and `server.ban(peer, "reason", seconds)` (0 = permanent, by address) remove
a peer and tell it why. Silence longer than `HealthPolicy::timeout_ms` (20 s, after startup grace) removes a peer; loss and latency only
flag it. `ServerConfig::max_peers_per_ip` (8 by default, 0 = no limit) stops one machine from taking every slot, and
`ServerConfig::handshakes_per_ip_per_s` (64) drops floods of proved handshake packets unanswered (a `ConnectRequest`, whose
source may be forged, is never charged). `NetServer::ban_ip` bans an address that never completed a join. Every per-address
rule (slots, bans, handshake budgets, the relay's lobbies) counts an IPv6 source by its /64 and an IPv4-mapped address as
its IPv4 address (ADR-3283); a ban entry in `bans.ron` with a bare IPv6 address bans its /64 (`/128` bans one address).

## Client

```rust,ignore
let socket = UdpTransport::bind("0.0.0.0:0".parse()?)?;
let mut client: NetClient<MyGame, _> =
    NetClient::connect(socket, server_addr, ClientConfig { name: "ada".into(), ..Default::default() }, &loadout)?;

loop {
    let now = elapsed_ms();
    client.receive(now);
    while let Some(event) = client.poll_event() { /* Connected, Ended(reason), Message */ }
    if client.is_connected() {
        // The ticks the server wants input for now, from its own feedback (ADR-8077).
        for tick in client.due_input_ticks(now) {
            client.send_input(tick, read_controls())?;
        }
    }
    client.flush(now);
}
```

Each input packet repeats the inputs the server has not acknowledged yet, up to `ClientConfig::input_redundancy`
(default 32), so a lost packet heals from the next and a loss burst heals as long as it is shorter than the lead. A
measured loss raises a lower setting to the copies 0.1 % residual loss needs (ADR-8312).

`due_input_ticks(now)` hands out each tick once, up to `estimated_server_tick(now) + input_lead_ticks()`. The server
reports how early or late each player's inputs arrive (a `LinkFeedback` with every tick sync and at once after a late or
missing input), and the client sets its lead from that: enough that 95 % of inputs arrive at least a tick before they
are needed, raised at once when they arrive late and lowered a tick after five calm seconds (ADR-8077). This keeps
inputs on time when the player's download is queued behind a big transfer, where the client's own round trip cannot
tell which direction is slow. The tick estimate never moves backward; it speeds up or slows down by at most 5 %, and
jumps forward only when it is more than 8 ticks behind. It starts on the server's tick: the accept anchors it with
the round trip the handshake measured, and the first lead already covers the way to the server, so a player on a
slow path stamps inputs for ticks they can still reach from the first frame (ADR-10096). `ClientEvent::InputsLate { margin_p5 }` tells a HUD that inputs
arrived late (the lead has already been raised), `client.link_feedback()` returns the last report, and `net.status`
shows all of it under `input_timing` (see [Net input (agents)](../agents/net-input.md#input-timing-feedback-clock-and-lead-adr-8077)).
If you stamp ticks yourself, use `client.estimated_server_tick(now) + client.input_lead_ticks()`.

Inputs of any size up to 16 KiB go through the same `send_input` (ADR-7896):

- One input datagram holds about 1.1 KB of inputs (`client.input_bundle_budget()`: 1153 bytes on a sealed link at the
  default 1200-byte packet cap). When the remembered inputs do not all fit, the newest ones go first and the older ones
  follow in up to three more datagrams, so a 548-byte order every tick still has eight ticks of cover per send. Small
  inputs keep the whole history in one datagram.
- An input that does not fit a datagram on its own (a long command list) goes on the reliable channel, split and
  reassembled for you, and reaches the server's tick driver on its tick. On a lost fragment it arrives one resend later
  (about 100 ms or more), so stamp it as far ahead as your other inputs. Inputs over `MAX_RELIABLE_INPUT_BYTES`
  (16 KiB encoded) are refused with `SendError::TooLarge`.
- A refused `send_input` changes nothing: send the next input as usual.
- `client.totals().input_bundles_trimmed` counts sends whose first datagram could not hold the whole history and
  `reliable_inputs` counts inputs that took the reliable channel (both are in `net.status`; see
  [Net input (agents)](../agents/net-input.md)). Builds from before the input feedback (wire version 3 and older)
  cannot join: they get `protocol_mismatch`.

## Channels

`Channel::GAME_RPC` messages (stream 2) arrive exactly once and in order; large ones (up to 256 KiB) are split and reassembled.
`Channel::Unreliable` messages fit one packet (about 1.1 KB), may be lost and are never resent: use them for state
that the next send replaces. `SendError::Backpressure` means a reliable fragment window or unreliable byte queue is full.
`ServerConfig::reliable_msgs_per_s` caps reliable sends per peer (off by default).

Reassembly is bounded, so a hostile peer cannot make the server hold memory (spec 120 § 5.2): a reliable fragment
longer than one packet of the link's `max_packet_bytes` fails the link (so both ends must use the same
`max_packet_bytes`); fragments stored ahead of a gap are capped per connection (`LinkConfig::connection_buffer_bytes`,
2 MiB, and never above one window of full fragments) and across a server's peers
(`ServerConfig::server_buffer_bytes`, default `min(64 MiB, 32 KiB x max_peers)`; over it a packet is refused as if
lost and resent); and a message whose next fragment does not come for `max(4 x RTO, 2 s)` while the peer keeps
sending fails the link as `reassembly_stalled` (on a lossy path that deadline stretches, up to 4x, with the peer's
measured packet loss, because an honest sender can lose several repairs in a row; ADR-8521). The server reports each as `ServerEvent::Rejected` with the peer's
name and drops it with `protocol_error`. An honest sender never trips them, even at 10 % loss. The shared game port
(`--shared-port`) and the Steam socket drop datagrams longer than `MAX_DATAGRAM_BYTES` (2,048) without copying them
and count them per source (`SharedPort::drops_by_source`, `SteamSocket::oversize_by_sender`); a send Steam refuses
is an error from `send_to`, counted in the endpoint's `socket_errors`.

### Reliable streams (spec 120 § 5.1, ADR-8302)

A connection carries up to 32 reliable streams. A loss on one never holds another back, and each stream keeps its own
order, ids, reassembly, window and stall clock. The engine owns ids 0-7:

| Id | Constant | Order | Priority | Carries |
|---|---|---|---|---|
| 0 | `StreamId::CONTROL` | ordered | 0 | session control: joins, travel, kicks (`NetSession`) |
| 1 | `StreamId::INPUTS` | unordered | 1 | the reliable input lane (inputs too large for a datagram) |
| 2 | `StreamId::GAME_RPC` | ordered | 2 | `Channel::GAME_RPC` game messages |
| 3 | `StreamId::REPLICATION` | ordered | 2 | the replication bridge's reliable frames (spawns, despawns, ordered messages) |
| 4 | `StreamId::LOCKSTEP` | ordered | 1 | lockstep control |
| 5 | `StreamId::CHAT` | ordered | 3 | chat |
| 6 | `StreamId::BULK` | ordered | 4 (background) | session streams and blobs (`send_stream`) |
| 7 | `StreamId::UNORDERED_RPC` | unordered | 2 | messages that need no order, and the replication runtime's `ReliableUnordered` messages (wire 6) |

Send on a stream with `Channel::Stream(StreamId::CHAT)`; `Channel::GAME_RPC` is the constant for
`Channel::Stream(StreamId::GAME_RPC)` (stream 2), and a message arrives as `Channel::Stream(id)` of the stream it was
sent on, stream 2 included. A game declares its own streams, ids 8-31, in
`LinkConfig::streams` (`StreamSpec::ordered(priority)` or `StreamSpec::unordered(priority)`); both ends must declare
the same table. An unordered stream hands over each message as soon as all its fragments are in, even past a gap.

Replicated games (`NetMessage` with `const CHANNEL: Channel = Channel::ReliableUnordered`): since wire 6 (ADR-10052)
the runtime sends these messages in their own frame on stream 7, in both directions, instead of inside the tick's
replication frame on stream 3. A lost replication packet then no longer holds them back: they arrive exactly once,
as soon as they arrive, and wait on the receiver only for their target entity (a message without a target runs at
once). A `ReliableOrdered` message still runs after its tick's spawns and state, in send order. One message larger
than about 15.5 KiB (`runtime::MAX_RUMSG_PAYLOAD`) rides the ordered frame instead: still exactly once, not unordered.

Higher-priority streams send first, repairs before new data. Background streams (priority 4 and lower, the bulk
stream) only fill the path's measured bandwidth-delay product, so a 256 KiB transfer on a 128 KB/s link leaves small
RPCs within 1.25x of their latency without it (it was 96x with one stream); the transfer itself goes at about a third
of the link until the rate controller lands. Each stream grants its sender a flow-control window
(`StreamSpec::window_bytes`, 256 KiB, bulk 1 MiB) and extends it as messages are consumed, so an honest sender never
makes the receiver store more than that ahead of a gap; a peer that does is dropped with `stream_over_cap`, and one that
names an undeclared stream with `unknown_stream`. A gap a stream's sender leaves unfilled fails the link as
`reassembly_stalled` after 4 to 16 probe timeouts of the peer's traffic (more on a lossy path, so an honest sender
always has enough repair attempts). `Connection::stream_status` and the `streams` rows of `net.status` / `net.peers`
show what each stream holds.

### The send scheduler, message caps and sockets (spec 120 § 4.3-4.5, § 5.3, ADR-8407)

Every poll fills packets in strict priority, inside the rate controller's allowance:

1. acknowledgments and flow-control credit;
2. essential unreliable data (inputs, tick sync, state acknowledgments), first in every packet, with a floor of its
   own in the pacer, so a reliable backlog never silences inputs;
3. the control stream (0);
4. repairs of the other streams, by stream priority;
5. new reliable data, by stream priority, with weighted round robin by fragment among the streams of one priority
   (`StreamSpec::weight`, default 1: stream 2 and stream 3 take turns);
6. other unreliable data (state frames, voice);
7. background streams (bulk), repairs first.

So state frames keep going while a bulk transfer fills the rest of the path: on `congested` with a 1 MiB transfer to
every client the snapshot staleness stays where it is without the transfer. Bulk data that has waited 250 ms
(`connection::BACKGROUND_STARVE_MS`) goes first in the next poll's first packet, so a join snapshot still arrives
when state frames take the whole path.

**Message caps per class.** Each stream starts at most `StreamSpec::msgs_per_s` messages in any one second (240 by
default, chat 30, bulk none). A game that queues 1,000 reliable messages in one tick does not flood the path: the
messages wait in the queue and leave at the cap, and nothing is lost. The stream rows of `net.status` / `net.peers`
name the class (`class`: `control`, `inputs`, `game_rpc`, `replication`, `lockstep`, `chat`, `bulk`,
`unordered_rpc` or `game`) and count `msgs_capped` (polls in which the cap held a message back) and
`msgs_last_second`. Set a game stream's cap with `StreamSpec::with_msgs_per_s` (`0` removes it); a transfer tool or
throughput test that knows what it sends sets `LinkConfig::message_caps = false`.

**Socket drops.** A datagram the operating system refuses (a full send buffer, `WouldBlock`) is not counted as sent:
it is `socket_drops` on the peer's link and in the endpoint `totals`, its packet number is not spent and its contents
stay queued for the next poll, so the receiver sees no gap and nothing is resent. A custom `DatagramSocket` must
return an error for a datagram that did not leave.

**Sockets.** `UdpTransport` asks for a 2 MiB receive buffer and a 512 KiB send buffer, reads both back and marks its
packets DSCP Expedited Forwarding (46) where the platform takes it; `net.status` shows the result as `socket`. If
either buffer reads back under half of what was asked, the process logs one warning: on Linux raise
`net.core.rmem_max` and `net.core.wmem_max` (Linux reports twice the size it was given). Windows takes the DSCP
option but applies it only under a QoS policy.

**Per tick, not per frame.** A client at 300 fps puts the same datagrams and bytes per second on the wire as at
30 fps (within 2 %): the replication bridge sends once per simulation tick, and acknowledgments, keepalives and pacing
run on the clock, not on how often the endpoint is polled.

### Names removed in v0.7 (spec 120 § 12.3, ADR-10067)

Two names deprecated in v0.6 are gone. Replace them like this:

| Removed | Use instead |
|---|---|
| `Channel::Reliable` | `Channel::GAME_RPC` (the constant for `Channel::Stream(StreamId::GAME_RPC)`, stream 2). Received stream-2 messages are now reported as `Channel::Stream(StreamId::GAME_RPC)`, which equals `Channel::GAME_RPC`, so `==` tests and `match` arms on it keep working. A `match` that had both a `Reliable` arm and a `Stream(_)` arm keeps its `Channel::GAME_RPC` arm first. |
| `congestion::CongestionNotches` | `congestion::LossNotches` (same `new`, `observe`, `budget`, `average_loss`, `min`, `max` and `NotchConfig`; same behavior). Size sends by `NetServer::send_allowance` first; the loss budget is only a cap below it. |

A serde document of your own that stored `Reliable` for a `Channel` must store `Stream(2)` instead.

## Testing without a network

`LoopbackNet` is an in-memory network: bind as many `LoopbackSocket`s as you like and step every endpoint with the
same millisecond clock. Every endpoint has a `NetConditioner` on its outbound side (`ServerConfig::conditioner`,
`ClientConfig::conditioner`, `set_conditioner`, or the agent method `net.conditioner_set`) with seeded loss,
duplication, delay, jitter, reorder and loss bursts, so a test of play under 8 % loss is deterministic.

The fuzz registry drives whole sequences, not single datagrams (ADR-8468): `net.connection_sequence` and
`net.sealed_endpoint_sequence` feed out-of-order fragments, duplicates, keepalive runs and, against a real sealed
server, replays, address changes and forged or oversized datagrams, and check each step against a model of the
reliable stream; `net.shared_port_sequence` and `net.voice_relay_route` do the same for the shared game port and the
voice relay. Run one with `cargo xtask fuzz run --target net.sealed_endpoint_sequence`, or from an agent with `fuzz.run`
(see [Fuzzing](../agents/fuzzing.md)).

Steam sessions test the same way without the SDK: put several `MockSteam` users on one `regia_steam::MockSteamNet`
and give them impaired links with `set_default_link(MockLink::profile("bad-wifi"))` or per user with `set_link`. The
profiles (`clean`, `good-wifi`, `bad-wifi`, `mobile-4g`, `transatlantic`, `congested`, `flappy`) have the soak
harness's numbers. Time is virtual: call `MockSteamNet::advance_ms` each frame with the same step as the sessions'
manual clocks, and `set_seed` replays the same drops. A clean link, the default, delivers every message at once and
in order. `link_stats()` counts what the links dropped, reordered and duplicated. `MockLink::profile_for_client(name,
index, duration_ms)` adds what the harness gives single clients (ADR-10069): `flappy`'s long outage (the removal timeout plus 2 s) on every fourth
client and `mobile-4g`'s NAT rebinding (a 500 ms reroute gap, `rebind_drops`). `mobile-4g` also has the harness's MTU
(1,232 bytes): a larger message travels as fragments and is lost when one is (`fragmented`).

`cargo run -p regia_net --example net_duel` runs a two-player race in one process over the loopback network with the
`wan` profile; `-- server 7777` and `-- client 127.0.0.1:7777 ada` run it over real UDP.

### Testing your netcode under bad networks

A game that only ever ran on a clean loopback has not been tested: real players are on Wi-Fi that drops packets in
bursts, on phones whose address changes, behind slow uplinks with deep queues, and next to players who send garbage.
Test the cases that drop players or make them lag out:

- **Bursty loss, not only random loss.** Real links lose several packets in a row. Your input redundancy and your
  timeouts must ride out a burst.
- **A path MTU below your packet size.** Mobile and VPN paths often carry no more than 1,232 bytes of UDP payload, and
  a bigger datagram is lost every time. Keep one datagram, headers included, under it. Send large data (a 4 KiB
  command, a level blob) as a reliable message, which the transport splits into fragments, not as one input.
- **Large inputs.** An input with a big command must never make every later send fail. Watch
  `NetClient::send_input`'s error. A client that cannot send input is stuck even though it is still connected.
- **A link that drops for a few seconds.** A 3 s Wi-Fi outage must not disconnect anyone. An outage longer than the
  timeout may, but the player must be back within seconds of the link returning.
- **An address change.** When a phone moves from Wi-Fi to cellular, the server sees the same encrypted session from a
  new address and must follow it.
- **A slow link.** When a client's downstream is below what your snapshots need, cut what you send to it
  (by the transport's allowance, `NetServer::send_allowance`) rather than fill its queue.
- **A hostile peer.** One client sending malformed, oversized or flooding traffic must be dropped, and nobody else may
  notice.

The engine's own soak does all of this to a sample game: `cargo xtask net-soak` (see
[Net soak (agents)](../agents/net-soak.md)). Its impairment network, `regia_net_soak::impair::ImpairedNet`,
is a `DatagramSocket` you can put under your own `NetServer` and `NetClient` in a test: per-direction latency and
jitter, random and Gilbert-Elliott loss, reordering, duplication, a bandwidth cap with a drop-tail queue, an MTU,
outages and NAT rebinding, all deterministic from a seed and driven by your test's clock. A link can also change
its numbers mid-run, move to another IP, run its clock fast or slow (`LinkSpec::clock_ppm`, then hand each endpoint
`LinkSpec::local_ms(now)`), or share its queue with a neighbor's download (`Direction::cross_bps`). Its named profiles
(`good-wifi`, `bad-wifi`, `mobile-4g`, `transatlantic`, `congested`, `flappy`, `hostile`) are the place to start.
For a specific worry there are more: `high-rtt` and `rtt-extreme` (very long paths), `rtt-step` and `wifi-to-cell` (a
path that changes), `clock-drift`, `nat-rebind` and `cgnat` (address changes and shared public IPs), `long-outage`,
`bufferbloat` and `asym-bufferbloat` (full queues), `downstream-loss` and `lossy-fat-pipe` (loss that is not
congestion), `mtu-1280` and `mtu-blackhole`, `listen-uplink` (a host's shared uplink), `flood` and
`forged-handshake` (attacks), and `mixed` (a realistic crowd). `cargo xtask net-soak --list` prints them all.

To see what those links do to *prediction*, use the time harness, `regia_net_soak::timed::TimedHub`: your setup and
gameplay systems on ECS worlds behind real `NetLink`s on the same network, each process ticking on its own clock (a
client can run 0.5 % fast), frames arriving between ticks, and the round trip coming from the transport's own
estimator. Its `TimeReport` counts corrections, resyncs, missing inputs and how far the input lead was off per client,
and the size of each correction (`correction_m`) and how many snapped (`snaps`).

**How many players.** `cargo xtask net-soak --plan scale` runs the sample game on a dedicated server at 40, 64 and 128
clients on `mixed`, with and without a hostile peer (`mixed+hostile`; any profile takes `+hostile`), and a listen
server at 16 and 32 on `listen-uplink`; `--wire udp` sends every datagram through real sockets on 127.0.0.1. Each run
records the server tick (and its budget: a quarter of the 30 Hz tick up to 40 clients, a third up to 64, half above),
the server's share of one core, its allocations per tick, its Mbit/s per seat, every client's rates against its
budget and why anyone left. The numbers per release are in `docs/perf/net-scale-soak.md`. Your game's simulation
shares the tick with this work, so measure your own server at your player count the same way: put the harness's
network under your server in a test, or run `--plan scale` against a copy of the sample with your tick rate.

What the netcode itself costs on one quiet core of a 2023 desktop CPU, with every link encrypted (v0.7, ADR-10081):
about 0.9 % of a core for 40 players, 1.6 % for 64 and 4.3-5.4 % for 128, with the server tick's p99 at 0.5, 0.8 and
2.0-2.7 ms. Most of what is left at 128 is encryption and the sample game's own snapshots (every avatar to every
client), so a game that sends each client less (interest management, rate tiers) pays less.

## Finding each other

- **Join codes and relay.** `relay::RelayServer` is a small rendezvous server. A host registers (`encode_register`,
  then `encode_register_with_cookie` with the cookie the relay answers with; a renewal does the same and keeps the code)
  and gets a six-letter join code; a player looks it up (`encode_lookup`) and gets the host's public address. The
  relay can introduce both for a hole punch (`encode_punch_intro`), and when that fails it forwards a bound flow
  (`encode_bind`, with the key the lookup returned) without reading it. `relay::simulate_hole_punch` shows which NAT
  pairs punch and which need it. A relay on a public address cannot be used as a reflector: requests are padded to at
  least the size of their answer, one `encode_list_games_page` request gets one directory page, control traffic is
  limited per source IP, a flow in use cannot be taken over, and each direction of a flow forwards at most 4 MiB/s.
  Join codes come from the relay's secret, so nobody can guess a live code from the seed or from other codes.
- **Many relayed clients (relay protocol 2).** A host behind a strict NAT can serve every player through the relay:
  each client sends `encode_allocate` (the host, the bind key from the lookup and its own random 16-byte allocation
  key; the relay answers `Challenge` first, send it again with that cookie) and gets `Allocated { port }`, its own port
  on the relay. The client then talks to `relay_ip:port` as if it were the server; the host gets `ClientJoined` and sees
  that client as the address `relay_ip:port`, one address per client, so nothing above the socket changes. Only the
  client and the host may use a port: anything else sent to it is dropped. After a NAT rebind, or when the host's game
  socket is not the one it registered from, send `encode_refresh` with your key to the port (answered `Challenge`,
  then `Allocated`). Caps answer `Full { cap }`: 40 clients per host, 8 allocations per source IP, the relay's port
  pool (4,096 by default; `RelayConfig`). A protocol 1 client still gets its one bound flow, and an allocation request
  in the old framing is answered `VersionTooOld`. Every relayed client shares the relay's IP, so raise
  `ServerConfig::max_peers_per_ip` (and `handshakes_per_ip_per_s`) for a relayed host. `--relay local` has 64
  allocation ports; the console command `relay` prints its counters.
- **Hosting and joining by code through the relay (ADR-8183).** Register from the game socket, not a socket of its
  own, so the address the relay records, hands out and relays to is the one the game uses (spec 120 § 10.7):
  `server_host::relay_split(socket, relay)` gives a game half for the server and a control half for
  `RelayRegistrar::new(Box::new(control), relay, &JoinTicket::from_game_socket(server.server_key()))`; call
  `server.add_relay_ip(relay.ip())` so relayed clients, who all share the relay's IP, are counted by address. The
  registrar renews every 15 s (that keeps the game socket's NAT mapping toward the relay open) and confirms each
  relayed client's allocation from the game socket (`ClientJoined`, then `Refresh`), which also works behind a
  symmetric NAT. The dedicated server does all of this for `--relay`. A client joins with
  `NetSession::connect(ConnectTarget::JoinCode("ABC123@relay.example.org:7800".into()), opts)` (or a bare code
  after `NetSession::set_relay`), or below the session with `server_host::JoinPath`: it looks the code up, tries the
  host's address, starts its own allocation after 500 ms and switches to it at 1.5 s if the host said nothing, all
  on one socket (`JoinPath::take_socket`). `NetSession::join_route` says which path won. A protocol 1 relay cannot
  carry many clients: when the direct path fails there the join ends with `relay_too_old` (also `relay_full`,
  `relay_no_allocation`, `join_timeout`, `join_refused`), never a hang. The relay's allocations, caps and refusals are
  the agent method `net.relay_status` ([Relay status (agents)](../agents/net-relay.md)).
- **NAT traversal: candidates and checks (ADR-8340).** On a relay of protocol 3 a join by code is ICE-shaped
  (RFC 8445): the client's first request names the code and its candidates (`nat::Candidate`: the game socket's own
  addresses, and port mappings through a `nat::CandidateSource`), and the relay introduces both sides at once: each
  learns the other's public address as the relay saw it, the other's candidates and a session key. Both then send
  connectivity checks (`RGNC` datagrams) from their game sockets to every candidate, best pair first, one per 50 ms
  per session and 5 ms apart across a host's sessions; a check from an address nobody announced is checked back at
  once (the way most home NATs are crossed). The handshake runs on the best pair that answered and the pair it
  completes on is used; if none completes by 1.5 s the relay allocation (started at 500 ms) is used instead. Two
  players behind NATs that map per destination ("symmetric", common on mobile carriers) always go through the relay.
  Nothing to set up: `RelayRegistrar` (on the game socket) and `JoinPath` do it; `RelayRegistrar::add_candidate_source`
  and `JoinPath::add_candidate_source` add a port mapping's candidate. `JoinPath::checks()` shows the pairs and their
  states; the host's counts are `net.relay_status`'s `nat`. An older relay simply skips the introduction.
- **Port mapping.** `port_map::try_port_mapping` asks the home router for a public port with PCP, then NAT-PMP,
  then UPnP IGD (`upnp::UpnpClient`; each step takes at most the client's timeout, and discovery only follows an
  answer to its own gateway search). Many routers refuse all three; fall back to the relay. The dedicated server's
  `--no-port-map` turns the automatic mapping off (`port_map_disabled`). `default_gateway_v4`
  finds the gateway on Linux, Android, Windows and macOS (ADR-8893): the netlink route dump, IP Helper's routing table
  and the `PF_ROUTE` sysctl, no administrator rights. Where the platform hides the table (some Android builds) or there
  is no IPv4 default route, hosting reports `port_map_unavailable`; pass the gateway in yourself.
- **Hosting: port mapping, port-forward check and capacity (ADR-8370).** A dedicated server with `--relay HOST:PORT`
  maps its game port on start: `port_map::PortMapWorker` races PCP, NAT-PMP and UPnP IGD on the default gateway
  (`race_port_mappers`: the first grant wins, a later grant is deleted again), renews the winner at half its
  two-hour lifetime and deletes it when the server stops; the host never waits on the router. The mapped address
  is announced to the relay as a `mapped` candidate. Once registered, the host asks the relay (protocol 3) to probe
  the mapped port from a relay port it never sent to (reachable or unreachable) and measures its uplink toward the
  relay for at most 3 s (a startup that doubles until the delivery rate stops growing); the client count is the
  uplink divided by the per-client budget (256 KiB/s by default). The console prints `port mapped ...` and
  `host check reachable ... clients N`; the agent method `net.host_check` has the details
  ([Host check (agents)](../agents/net-host-check.md)). In your own host:
  `RelayRegistrar::start_host_check(HostCheckConfig::default(), now_ms)`, then feed it the worker's events with
  `set_port_mapping` / `clear_port_mapping`, and read `RelayRegistrar::host_check()` before you open a big match.
  Running your own relay: `RelayServer::bind` adds the probe port; a relay built from sockets needs
  `RelayServer::with_probe_socket`, or hosts report `relay_cannot_probe`.
- **LAN browser.** `discovery::ServerAnnounce` is a small broadcast datagram with the protocol id, port, player
  counts, a name and your own opaque card. `discovery::DiscoveryConfig` chooses the announce magic and the UDP
  port. The defaults stay `RGNA` and UDP 7900. Two announcers may share a port; each one keeps only datagrams
  with its own magic (`ServerAnnounce::encode_with` / `decode_with`). `LanSocket` is the UDP socket
  (`SO_REUSEADDR`, so two sockets can bind one port). `LanSocket::poll` gives each announce with its sender's
  address; join `SocketAddr::new(from.ip(), announce.game_port)`. One poll reads at most 256 datagrams
  (`LAN_POLL_MAX_DATAGRAMS`). `LanAnnouncer` on a `DiscoveryHub` is the in-process bus.
- **Address join codes.** `mint_address_join` / `encode_address_join` / `decode_address_join` carry an IP, a
  port, a nonce and an expiry beside the six-character relay codes. The default lifetime is 12 hours
  (`ADDRESS_JOIN_TTL_DEFAULT_S`). The code uses the same typo-resistant alphabet as a relay code and is not a
  secret: the host password stays the access control. A code is valid while `now < expires_at` and expired at
  `now >= expires_at`. A one-character typo fails the checksum (`address_code_invalid`); a relay code is not an
  address code. The nonce and the expiry are advisory: only the decoding client checks them, and the host
  cannot revoke an address code (it holds no state for it; change the password or the port). A relay join code
  can be withdrawn: `RelayRegistrar::revoke_code`, or the dedicated server's console `revokecode` (ADR-10069).
- **Store ticket hook.** `NetServer::set_ticket_check` installs a `JoinTicketCheck`. It runs after the password
  gate, after `NetGame::validate_join`, and after the live-peer, per-address and capacity checks, immediately
  before the peer is added. A refusal ends the client with `ticket_refused`. A pending verdict can be kicked
  later (`disconnected:kicked`). The server ends the store session when the peer leaves. No hook leaves
  admission unchanged. `NetGame::join_ticket` defaults to an empty slice. Join data is at most 512 bytes.
  Steam's check is `regia_steam::SteamJoinTicketCheck` (an in-memory allow-list in tests; `BeginAuthSession`
  only when the `steamworks` feature is on). The host pumps Steam `run_callbacks`. Late refusals are applied in
  `NetServer::receive`, so a `receive` + `flush` loop kicks too. A client that restarts on a new port is admitted
  and its stale peer kicked. The hook installs on every server type (ADR-10069): `NetLink::set_ticket_check`
  (clients send the ticket with `NetLink::connect_with_join`), `NetSession::set_join_ticket_check` (the ticket is
  `ConnectOptions::auth`; checked after the password, on resumes too, a late refusal kicks with `ticket_refused`)
  and `ServerHost::set_join_ticket_check` for the dedicated server. A Steam refusal that belongs to a session a
  restart replaced no longer kicks the new one.

## When versions or content differ

Set `const GAME_VERSION: &'static str = "1.4.2";` in your `NetGame` (dotted numbers order releases). A client of another
release or other content is refused with `protocol_mismatch`, and the refusal says which side is older: the client gets
`ClientEvent::VersionMismatch { server_game_version, ours, server_is_older, side, message }` (a `NetLink` game gets
`LinkEvent::VersionMismatch`) right after the `Ended` event. Show `message` to the player; `server_is_older == Some(false)`
means "update the game", `Some(true)` means the server is older than this build. A build that predates this check is still
named: its plain refusal starts the version probe, and no answer within 1.5 s means the server is older.

Content known only at run time (a mounted data set) goes into `NetServer::set_content_hash` and
`NetClient::set_content_hash`, or `LinkOptions::content_hash` on a `NetLink`, which also checks the replicated components
you registered: a client with other components or content is refused (`content_differs`) instead of joining.

Every server also answers the plaintext version probe (`docs/formats/version-probe.md`), rate limited per network prefix,
so a server browser or a support tool can read a server's versions without joining.

## Diagnostics

`server.overlay()` returns a `DiagnosticsOverlay` (per-peer round-trip time, jitter, loss, kbps, missed inputs,
flags) with `lines()` for a debug HUD; `server.status()` and `peer_rows()` are the same data the agent methods report
(see [Networking (agents)](../agents/net.md)).

## A server without the ECS

A game that keeps its own simulation (a lockstep game, a headless server in its own crates) can use `regia_net` without
the ECS: turn the default `ecs` feature off where the dependency is declared. In a workspace that is the
`[workspace.dependencies]` entry; a member's `default-features = false` beside `workspace = true` is an error in edition
2024 when the workspace entry keeps the defaults ("cannot override workspace's `default-features`").

```toml
# Workspace Cargo.toml
[workspace.dependencies]
regia_net = { version = "…", default-features = false }

# A member that needs the ECS half turns it back on:
# regia_net = { workspace = true, features = ["ecs"] }
```

A direct path or git dependency takes it in place: `regia_net = { path = "…", default-features = false }`.

That build has the transport, `NetServer`/`NetClient`, the tick driver, the session core (`SessionServer`,
`SessionClient`, bans, the content manifest, streams), the lobby core, relay, join codes, port mapping, discovery and the
snapshot delta codec, and links no `regia_ecs`, `regia_app`, `regia_scene`, `regia_asset` or `regia_platform`. Replication,
prediction, interpolation, lag compensation, the recorder, `NetSession` and `NetSessionPlugin`, `LobbyPlugin`, `NetLink`
and the dedicated-server host need `ecs`, as does the `voice` feature (which turns it on). The `net.*` agent methods read
`NetAgentState` from an app, so they need `ecs` in the game; without it, read `server.status()` and `peer_rows()`
directly. The example at the top of the `regia_net` crate docs (a loopback `NetServer` and `NetClient`)
builds either way. ADR-8032, spec 120 § 3.2.

### Using regia_net without the ECS

What the core build includes (spec 120 § 3.2):

- **Endpoints and handshake:** `NetServer` and `NetClient` over any `DatagramSocket` (UDP, `LoopbackNet`, the conditioner),
  the challenge/token handshake, version and content checks (`NetGame::content_hash`), passwords, bans and address blocks.
- **Crypto:** the encrypted, authenticated transport and known-host pinning.
- **Tick driver:** `TickClock` plus the server's `advance_tick`, per-tick inputs with input lead and missed-input counts.
- **Rate controller and link:** the reliable and unreliable channels, congestion control and path MTU probing.
- **Relay, join codes, port mapping and NAT traversal:** relay allocations, `join_code`, UPnP/port mapping, discovery and
  candidate checks.
- **Snapshot delta:** `SnapshotEncoder` for state you replicate from your own simulation.
- **Lockstep and desync:** `LockstepHost` and the desync hash and probe.
- **Session and lobby cores:** `SessionServer`, `SessionClient`, the content manifest, streams and the lobby hub.
- **Net messages:** `#[derive(NetMessage)]` and `NetMessageRegistry`. Without the ECS there is no `add_net_message` or
  `NetSender`: register the types on a `NetMessageRegistry` in the same order on every process, send a registry index plus
  `codec::encode_to_vec` bytes inside your game's `NetGame::Message`, and read them back with `registry.decode(index, bytes,
  &CodecContext::detached())` and `downcast`. `registry.validate(index, &msg, &ValidateContext::core(from, tick))` runs the
  type's `validate`. Fold `registry.hash_into` into `NetGame::content_hash` so a client with another message set cannot join.
  `Entity`, `Handle<T>` and `AssetRef<T>` fields need `ecs`.

The example `crates/regia_net/examples/net_core_headless.rs` puts these together: a headless server and two clients on the
in-memory loopback network, the tick driver, per-tick inputs and a derived standings message broadcast every ten ticks.

```text
cargo run -p regia_net --no-default-features --example net_core_headless
```

`cargo xtask check-net-core` keeps this honest, and `cargo xtask preflight` and CI run it: `cargo tree -p regia_net
--no-default-features` must reach none of `regia_ecs`, `regia_app`, `regia_scene`, `regia_asset`, `regia_platform`,
`regia_audio` or `bevy_ecs`; the core and the example must build without `ecs`; and the core's unit tests and the example's
two-client session must pass without it. ADR-8501.

## Limits in this phase

- Traffic to every non-loopback peer is encrypted and authenticated by default; loopback stays plain. A client that does not
  pin the server's key is protected from a listener, not from an attacker in the middle. See
  [Net transport encryption](../agents/net-crypto.md).
- State replication, interpolation and prediction are built (sections below); lag compensation is built too (`regia_net::lagcomp`, `book/agents/net-lag-compensation.md`).
- A protocol 1 relay client (`encode_bind`) still gets one relayed flow per host address; protocol 2 clients use allocations (above). A join by code does not fall back to a protocol 1 bound flow: it ends with `relay_too_old` (ADR-8183). Candidate checks from the game socket are built (the NAT traversal section above, ADR-8340): host and server-reflexive candidates, paced checks, nomination of the first sealed pair, and the relay allocation when no direct pair completes by 1.5 s. A mapped candidate is added through `nat::CandidateSource` when a port mapping exists; gathering those mappings is a later lane.

## Replicating entities (specs 70 and 71)

On top of the transport, `regia_net` replicates ECS entities from the server to its clients (ADR-1643):

```rust,ignore
use regia_ecs::prelude::*;
use regia_net::prelude::*;
use regia_net::*;

#[derive(Component, Clone, Debug, Default, Replicate)]
struct Health {
    hp: u32,
    #[replicate(quantize(min = 0.0, max = 1.0, bits = 8))]
    shield: f32,
}

#[derive(Clone, Debug, Replicate)]
struct Fire { power: u32 }
impl NetMessage for Fire {
    const DIRECTION: Direction = Direction::ToServer;
}
impl ToServerMessage for Fire {}

fn build(app: &mut regia_app::App) {
    app.add_plugins(NetPlugin { mode: NetMode::ListenServer });
    app.replicate::<Health>(ReplicateOptions::default()); // same order on every process
    app.add_net_message::<Fire>();
}
```

- Spawn an entity with `Replicated::default()` on the server; every client gets a copy with the registered
  components, and only changed fields travel afterwards.
- Ownership decides who may send input and to-server messages: `NetOwner(peer)` on the server; clients see
  `NetOwnedLocally` on their own entities.
- Gate systems with `on_server()` / `on_client()`; send with the `NetSender` system parameter; read with
  `Res<NetMessages<Fire>>`. The same code runs offline, as a listen host and as a dedicated server.
- Over the network, set `type Message = regia_net::bridge::NetFrame;` in your `NetGame` and call
  `ServerBridge::pump_in` after `server.receive(now)` and `pump_out` before `server.flush(now)` each frame (the same
  with `ClientBridge` on clients).
- Test multiplayer in one process with `regia_net::testing::LoopbackHub`: N worlds at fixed steps, seeded loss,
  delay, duplication and reordering.
- Project settings use the `config/net.ron` format (`docs/formats/net-ron.md`): parse it with `NetSettings::from_ron` and insert the resource before the net plugins; the file is not loaded automatically.

The agent pages are [Net model (agents)](../agents/net-model.md) and [Net replication (agents)](../agents/net-replication.md).

## Relevance, priority and interpolation (specs 72 and 74)

A big world does not send everything to everyone. On the server, turn the relevance pipeline on and give entities
profiles; on clients, turn interpolation on so other players' entities move smoothly between updates.

```rust,ignore
use regia_net::*;

// Server: who sees what, and what goes first.
regia_net::install_net_interest(app.world_mut());
app.world_mut()
    .resource_mut::<NetProfiles>()
    .load_ron("tank", include_str!("../assets/net/tank.netprofile.ron"))
    .expect("valid profile");
// A player's camera or character is its viewpoint.
commands.spawn((NetViewpoint { player }, transform));
// Entities name their profile.
commands.spawn((Replicated { profile: Some("tank".into()), ..Replicated::default() }, tank_transform));

// Client: show proxies a short delay in the past, blended between received states.
app.add_plugins(NetInterpolationPlugin::default());
```

- A `.netprofile.ron` sets the priority, a spatial radius (or `World`), linger, send intervals and the
  interpolation fields; built-ins are `default`, `player`, `projectile`, `prop`, `held` and `game_state`
  (`docs/formats/netprofile-ron.md`).
- Owned entities are always relevant to their owner; `NetVisibility` and `NetGroupVisibility` deny or allow peers
  and groups; `net_grant_relevance` reveals an entity for a while; `app.add_net_relevance_filter::<F>("name")` adds
  a game rule (line of sight, audibility).
- Fog of war for a server-authoritative game: add `regia_framework::fog_relevance::FogRelevancePlugin`, put each
  connection on a team in `FogTeamOf`, and give fogged entities a `FogUnit` and the `fog` profile. A client is then
  never sent an enemy its team cannot see (anti-maphack), with a few ticks of linger and lead so units do not pop. In
  a lockstep game every peer holds the full state, so fog there is presentation only. See
  [Fog of war](../agents/fog-of-war.md) § "Fog over the network".
- Each connection has a byte budget (`replication.client_rate_bytes`, never above what the transport's rate controller
  allows); entities with changes queue by priority, so near and owned entities update first and every changing entity
  is eventually sent. New entities, a joining player's own entities and entities forced by `max_interval_ticks` wait
  for the budget too, so a burst of spawns fills in over a few ticks instead of flooding the link; a large record that
  overshoots one tick is paid back from the next ones.
- Keep one entity's replicated state under 16 KiB. An entity that grows past it is taken off every client (they get a
  despawn) and comes back when it fits again; the server logs `net.state_too_large` once each time it crosses the
  limit, and `net.state_channel` lists it in `too_large_entities`. Put big data in a `NetVec` or on child entities.
- On clients, query out `NetPendingDespawn` in gameplay (a despawned proxy stays until its last moment is shown);
  `NetStarved` marks a proxy whose data stopped.
- Interpolation copes with a bad link and a full budget (spec 120 § 9.7): the delay grows by the measured burst-loss
  run (up to `max_delay_ms`), so a few lost packets in a row no longer make proxies extrapolate; and an entity the
  budget sends only every few frames moves at its own pace between updates (its velocity is inferred from its last
  two states when it has no `NetVelocity`), instead of stopping and starting. Replicate `NetVelocity` on movers that
  stop abruptly: without it a deferred entity that stops overshoots by up to one of its update intervals, then
  settles back.
- State goes out on ticks that are a multiple of `replication.send_every_ticks` (default 2, so 30 times a second at
  60 Hz). Messages you send on other ticks leave with the next send tick.
- Spawns and deltas travel on the unreliable channel, each delta against the state the client acknowledged, so a lost
  packet delays only the entities in it. Clients on an older protocol keep a reliable stream by themselves, and
  `replication.state_channel: Reliable` puts everyone there, which is handy when diagnosing a bad link.

### Large worlds: distance tiers and combat heat

Relevance work follows what changed, so a busy server stays cheap (spec 120 § 8.4): each viewpoint searches each
profile's radius separately (one long-range radar profile does not widen the search for everything else), a held
entity is looked at again only when it or the viewer moved, and a spawn, despawn or ownership change touches only
the connections and copies it concerns. `net.bandwidth` shows the work of the last update under `interest`.

Far things can also update less often. Give a profile distance send-rate tiers (spec 120 § 8.3):

```ron
NetProfile(
    format_version: 2,
    relevance: Spatial(radius: 1000.0, hysteresis: 0.15),
    // Near (< 150 m): every send. Mid (150-600 m): every third send. Far: twice a second.
    rate_tiers: Some((near_m: 150.0, mid_m: 600.0, mid_every: 3, far_hz: 2.0)),
)
```

Owned entities, spawns and records forced by `max_interval_ticks` never wait for their tier, and a held-back entity
keeps gaining priority, so it goes first on the tick it may. `net.bandwidth` counts the held-back updates
(`tier_held`) and the sends per tier (`tier_sent`).

Fights should win the budget. When an entity deals or takes damage, raise its combat heat on the server:

<!-- ignore-reason: `world` and `entity` are the game's server world and the entity that was hit -->
```rust,ignore
regia_net::net_note_combat(world, entity, 1.0); // one notable hit
```

Within 50 m of a viewer (`NetCombatHeat.radius_m`) its priority grows by `1 + heat × (1 − distance / radius)`;
the heat halves every 2 seconds. A `.netprofile.ron` written before this change (`format_version: 1`) loads as it
is; `regia upgrade-project` rewrites its version number.

### Compact transforms (spec 120 § 8.2)

A moving entity's transform travels as integers: its position in whole steps of its profile's `position_step_m`
(default 1 mm, so a client's position is within half a millimetre of the server's), its rotation as smallest-three at
10 bits, and its velocity (`NetVelocity`) in 1/32 m/s steps. Each update is a delta against the state the client
acknowledged, with one bit for every part that did not change, so a walking character costs about 9 bytes per update
instead of about 36, and the default budget carries about three times as many moving entities per send tick.

- Set `position_step_m` coarser in a profile for entities that do not need millimetres (props, far vehicles), or
  `None` to send raw `f32` positions.
- Velocities past 64 m/s, positions past about 2,000 km from the origin at 1 mm, and replicated children travel raw.
- This changed the replication protocol to version 4: a client and a server must both be built with it. An older
  build is refused at the handshake (`protocol_mismatch`), never misread.

#### The extrapolation gate (optional)

A profile can send moving entities only when the client's picture of them drifts: set `extrapolation_gate_m:
Some(0.02)` in its `.netprofile.ron` (format 2) and the server holds a transform-only change back while the client's
extrapolation of what it already has (the last record moved along its velocity) is within 2 cm and 0.5 degrees. Clients
keep extrapolating such entities between records instead of stopping after `max_extrapolate_ms`. A walker on a straight
line is then sent about once a second instead of every send tick, and a crowd that turns costs several times fewer
bytes; the price is that a copy can be up to the gate (plus a few millimetres while a correction travels) away from the
server, instead of within half a `position_step_m`. The gate is off by default. Changes to other replicated
components, teleports and predicted entities are never held back. Details: [net extrapolation gate
(agents)](../agents/net-extrapolation-gate.md).

### Smaller packets: entropy coding

Beyond deltas and quantization, the server can range-code each state frame's body (spec 72 § 18, ADR-3315) — a
median of a few percent to about 9 % fewer state bytes on the measured workloads, most on frames that pack many
entities. It is off by default:

```ron
// config/net.ron
replication: (
    // ...
    entropy: Range,   // Off (default) | Range
),
```

- A dedicated server's `server.toml` can override the project with `[net] entropy = "off" | "range"`.
- The stage negotiates per connection: only clients speaking replication protocol 3 get coded frames, and protocol 2
  clients receive byte-identical protocol 2 frames.
- Only entity state is compressed; handshake, passwords, chat and game messages never are. Coded frames that would
  not shrink fall back to raw within the same negotiated frame.

The agent page is [Net entropy coding (agents)](../agents/net-entropy.md); `net.entropy_stats` reports the per
-connection before/after byte counters.

### Long round trips keep their deltas

The server sends each entity as a delta against the last version the client acknowledged, and the client keeps the
versions the server may still name. That store holds up to 512 versions per entity (ADR-10051), enough for an entity
that changes every tick on a 3 s round trip at 128 Hz, so a player on a very slow path still gets deltas, not the full
state every time (which is what a 64-version store did above about one second of round trip at 60 Hz). A client on an
ordinary path stores far fewer: about one round trip of versions plus those of 8 to 16 ticks per changing entity
(9 to 10 on a 12 ms path at the default send rate, where it used to be 64). Nothing to
configure. `net.state_channel` on a client shows `longest_ring` and `version_bytes_stored`; `baselines_expired` rising
on the server with little loss means a round trip beyond the 512 versions.

The agent pages are [Net relevance (agents)](../agents/net-relevance.md),
[Net interpolation (agents)](../agents/net-interpolation.md) and
[Net state channel (agents)](../agents/net-state-channel.md).

### A sim outside the ECS: the snapshot codec

A game whose simulation lives outside the ECS can send whole-world snapshots with the standalone codec
(`SnapshotEncoder` / `SnapshotDecoder`, see [Snapshot delta codec](../systems/snapshot-delta.md)). Feed the decoder
packets as they arrive: a late or repeated packet never moves the shown world back, and after an ack outage longer than
the baseline ring a budget-limited full packet is merged instead of wiping entities (`is_rebasing()` until the stream has
re-sent everything). Call `SnapshotDecoder::reset()` when the client reconnects to a new server process.

For more than a handful of entities or clients, put `ExternalReplication` on top of the codec: it gives the sim the
relevance, priority, distance rate tiers and per-client byte budget the ECS path has, with the same grid and tier code,
and needs no ECS (spec 120 § 8.6):

<!-- ignore-reason: `Units` is the game's own simulation; the trait bodies are left to it -->
```rust,ignore
use regia_net::external_replication::*;

struct Units; // the game's own sim
impl ExternalSource<u32> for Units {
    fn priority(&self, _key: u32, _client: PeerId, _distance: Option<f32>) -> f32 { 1.0 }
    fn record(&self, key: u32, out: &mut Vec<FieldValue>) { /* one value per schema field */ }
}

let config = ExternalConfig {
    send_every_ticks: 2,
    rate: 32_000, // bytes per second per client
    classes: vec![ExternalClass {
        relevance: RelevanceShape::Spatial { radius: 800.0, hysteresis: 0.1 },
        rate_tiers: Some(RateTiers::default()), // near every send, mid every 3rd, far at 2 Hz
        ..ExternalClass::default()
    }],
    ..ExternalConfig::default()
};
let mut rep = ExternalReplication::<u32>::new(schema.clone(), config)?;
rep.add_client(peer)?;
// Every tick: positions of what moved, viewpoints, then one packet per client.
rep.upsert(unit_id, position, 0)?;
rep.set_viewpoints(peer, &[camera])?;
rep.send_tick(tick, &units, &mut packets)?;
// When the client's ack arrives (ExternalAck::from_bytes):
rep.ack(peer, ack)?;
```

On the client, `ExternalInterpolator` decodes each packet, gives the ack to send back (`ack().to_bytes()`, 16 bytes:
the newest baseline plus which recent packets it applied, so a lost record is re-sent and a delivered one is not), and
`sample(key, &mut fields)` blends each entity between its received states. Show `sample` or `latest`, not `current()`:
an entity the server held back for its tier stays at an older state in the codec's world. If the congestion controller
gives an allowance, pass it with `set_allowance`; the budget is the smallest of the project rate, the rate the client
declared and the allowance. To let agents see the replication, publish its stats now and then under a label:
`rep.publish_stats("world", tick, &mut world.resource_mut::<NetExternalStats>())` (insert the resource once); agents read
them with `net.external_stats` (ADR-8892). The agent page is
[Net external replication (agents)](../agents/net-external-replication.md).

## Predicting your own character (spec 73)

With a round trip of 100 ms, waiting for the server before moving feels broken. The owning client therefore runs its
own entities ahead of the server from its own input, and the server stays the authority:

```rust,ignore
#[derive(Clone, Debug, PartialEq, Default, Replicate)]
struct Pad { x: i8, jump: bool }
impl NetInput for Pad {
    fn on_missing(last: &Self) -> Self { Self { jump: false, ..last.clone() } }
}

app.add_net_input::<Pad>()
    .predict::<Transform>(Tolerance::position(0.01))
    .predict::<Motion>(Tolerance::Exact)
    .add_predicted_systems(step_hero);

// The server marks the hero predicted by its owner.
commands.spawn((Replicated::default(), hero_transform, Motion::default(), NetPredicted, NetOwner(peer)));
```

- Write the input each fixed tick into `InputCommand<Pad>` on the controller entity (an entity with `NetPlayer`);
  the server writes the command it consumed into the same component, so `step_hero` reads input the same way on both
  sides. Use `PredictionCx::simulates(role)` to skip entities this process does not step, and
  `PredictionCx::owner_player(owner)` to find whose input drives an entity.
- The step must be deterministic: the same input and start state give the same result. It runs again when the
  server disagrees (a rollback), so gate sounds and effects on `PredictionCx::first_time()`.
- Give movers that players stand on a `TickDrivenMotion` (keys over time, a function of the tick) so riders predict
  exactly; replicate them with `TransformReplication::None`.
- `PredictionCx::spawn_predicted(player, bundle)` shows a projectile at once; the server's spawn binds to it, or it
  is removed with `NetPredictedSpawnRejected` when the server refused the shot.
- Corrections jump the simulated pose and smooth the rendered one (`NetVisualOffset`).
- Write `InputCommand` once per fixed step even when nothing changed. Regia latches it per step and delivers it on
  exactly one tick: when time dilation runs no tick in a step the input waits for the next one, and when it runs two the
  second gets `on_missing`, so a one-step jump press fires once. A command that reaches the server after its tick is
  merged into the next one. Give an input with presses a `merge_late` that keeps both inputs' presses and the newer
  analog values:

  <!-- ignore-reason: fragment of the Pad input above; merge_late is the game's own field rule -->
  ```rust,ignore
  impl NetInput for Pad {
      fn on_missing(last: &Self) -> Self { Self { jump: false, ..last.clone() } }
      fn merge_late(late: &Self, next: &Self) -> Self { Self { jump: late.jump || next.jump, ..next.clone() } }
  }
  ```

  On the `NetServer`/`TickDriver` path implement `NetCodec::merge_late` the same way; a late input reaches the next
  tick's `TickInputs`.
- Prediction stays on at high round trips (tested at 450 and 650 ms): how far back a correction re-runs grows with
  the measured round trip, up to `prediction.history_ticks` (64 ticks, about 1 s at 60 Hz; up to 128). A correction
  older than that still moves to the server's state and re-runs your inputs from there, counted as a reset in
  `net.prediction`. Raise `history_ticks` only if players regularly see `resets` at round trips over a second.

The agent page is [Net prediction (agents)](../agents/net-prediction.md).

### Characters

`NetCharacterPlugin` does this for characters: spawn `NetCharacter` with `Replicated`, `NetPredicted` and `NetOwner`, write
`InputCommand<CharacterNetInput>` each tick, and the movement step runs in the predicted schedule on both sides. A character
keeps its place on a replicated moving base, so a raft that reaches the client late causes no correction, and root motion
comes from a replicated `NetMontagePlay` that re-runs exactly. See [Net characters (agents)](../agents/net-character.md).

### Hitting what you saw (lag compensation)

With `LagCompPlugin` on the server, a shot is judged against the targets as the shooter's screen showed them: at the
view tick its command carried. The server checks that tick against the shooter's own measured window, the round trip
plus input lead plus interpolation delay plus one tick and a small allowance (spec 120 § 9.6): a player on a 5 ms
link cannot claim a view a quarter second old, and a view tick that runs backward or into the future is clamped and
counted (`lagcomp.stats` `windows`). `lag_compensation.max_rewind_ms` (default 1,000 ms, at most 1,000) caps every
window: lower it to favor targets over shooters on very long links. See
[Net lag compensation (agents)](../agents/net-lag-compensation.md#the-rewind-limit).

## Destruction

Destructible objects break the same way for everyone: the server decides, clients replay. Add
`regia::framework::destruction_net::DestructionNetPlugin` after `NetPlugin` and `DestructionPlugin`, register each
destructible asset on every process, and place instances on the server only. Late joiners catch up from a
snapshot. See [Destruction over the network](../agents/destruction-net.md).

## Keeping a server's world

A dedicated server can keep its world across restarts, for persistent co-op and survival servers. Turn it on in
`server.toml` (format version 2):

```toml
[persistence]
enabled = true
autosave_ticks = 18000            # every 5 minutes at 60 Hz
keep = 5
```

The server then saves everything the save framework saves (entities marked `Saveable`, saveable resources, save
sections) on a tick schedule, when a player leaves, before a level change and on a graceful shutdown, each save written
atomically into `saves/<level>/` with the oldest beyond `keep` removed. On start it restores the newest save that reads
back before any player can join; a damaged newest save is set aside and the one before it is used. Register the
components a returning player should get back:

<!-- ignore-reason: a fragment of a server's App setup; Inventory is the game's own component type -->
```rust,ignore
app.register_player_persistent::<Inventory>()
    .register_player_persistent::<Transform>(); // the pawn's position
```

They are kept per player identity (the platform id from the authenticator, such as a Steam id) and come back when that
player joins again, also after a restart. The console has `save`, `persistence` and `restore <slot> confirm`. See
[Server world persistence (agents)](../agents/net-server-persistence.md).

## Recording and instant replays (spec 103)

A server can record what its clients receive. `regia_net::recorder::start_recording(world, path, RecorderConfig::default())` adds a virtual connection that owns nothing, so what an owner alone sees is not recorded and real clients receive the same bytes as without it. `stop_recording` closes the file and it appears at `path` only then; a world dropped by a clean exit closes it too, and a crash leaves only a tmp file. `start_ring` keeps the last seconds in a fixed block of memory, and `export_ring` writes them out, for kill-cams and instant replays.

`regia::framework::net_replay::open_replay(world, path, ReplayOptions::default())` plays a recording in any world with the same components and messages registered in the same order, with no server: copies spawn from your prefabs, proxies interpolate and your message handlers run. `NetReplayPlayer` has play, pause, seek, speed and a free or follow camera. A build with another protocol refuses the file. See [Network replay (agents)](../agents/net-replay.md).

## Host migration

The engine does not migrate a session to a new host (spec 70 § 1, spec 77, spec 89): when the host leaves, the
session ends for its clients (`server_shutdown` when the host closes, `timeout` when it vanishes). A game that wants
the match to go on picks the new host itself, starts a server there and has the other players join it again.
Reconnect grace (`session.reconnect_grace_s`) only resumes a dropped client on the same host; see
[Net session (agents)](../agents/net-session.md).

### Silence, interruption and outage recovery

New sessions wait 20 seconds of silence before dropping a peer. After 1.5 seconds without a
validated packet, they report an interruption while keeping the connection and player slot.
Startup gets a 30-second floor for a peer that has not started gameplay. High RTT and loss
never trigger removal by themselves. Configure both ends with the same policy:

```ron
// config/net.ron, format_version: 3
session: (interrupt_ms: 1500, timeout_ms: 30000, start_grace_ms: 30000),
```

`NetSession` resolves these settings into `TimeoutPolicy` and passes them to its transport.
For `NetLink`, set `LinkOptions::timeout`; for a dedicated host, set the same three millisecond
keys under `[net]` in `server.toml` v4. Connect, authentication, level load and join deadlines
are separate. Legacy `SessionSettings::timeout_s` works in code when `timeout_ms` remains at
20000; a nondefault millisecond timeout wins. In `config/net.ron` a `timeout_ms` key always
wins, and `timeout_s` is read only from files without one. Values out of range are clamped
(`timeout_ms` at least 1000, `interrupt_ms` 100 to below `timeout_ms`, `start_grace_ms` at most
600000); `server.toml` refuses them. `regia upgrade-project --write` migrates older files.

Handle `PeerInterrupted` / `PeerRestored` in transport, link or session events to show and clear
a connection-problem indicator. Interruption does not change the lifecycle phase, peer identity
or occupancy. `net.status`, `net.peers` and `session.state` expose `interrupted`; the diagnostics
overlay labels it `INTERRUPTED`. The bridge/session adapter attaches `NetInterrupted` to the
interrupted peer's owned entities and player controllers on the server. Game movement on the
server should filter it out; it is removed when traffic returns, when the peer leaves, and when
the entity changes owner. Replicating it to other clients, where it holds proxy interpolation, is
opt-in: call `NetPlugin::replicate_interrupted_marker(&mut app)` on every process. It changes the
protocol hash, so v0.5.0 peers and replays need the same choice.

Reliable fragments back off on consecutive timeouts, up to 8 seconds by default
(`LinkConfig::resend_backoff_max_ms`). Newly acknowledged traffic resets the wait. Recovery
starts with four reliable-bearing packets per poll, grows with acknowledged rounds, and caps
reliable traffic for at least the first second of restored traffic. Empty recovery ACKs use
keepalive cadence, but a packet that carried data is acknowledged at once. This bounds the
outage backlog; the later rate-controller work controls arbitrary offered application traffic.
`link.recovering` and `link.recoveries` expose recovery activity. Idle interruption probes
back off from 250 ms to 4 seconds, with deterministic jitter.

What counts as an outage, and how recovery is paced, is part of one transport policy
(ADR-7995, see "Loss, pacing and recovery" below). In short: a hitch is not an outage.
Recovery starts only when a fragment has timed out twice in a row on a measured path, or when
packets sent that long apart turn out lost. Its per-second cap is the larger of 32 packets and
half the rate the path delivered before the outage, and it ends at once, with the earlier
window, when the peer turns out to have received everything (a client that stalled in a level
load, not a dead path).

Only the client backs off to probes while interrupted. The server keeps its keepalive floor, so
a client that still hears the server never sees gaps longer than its own interruption
threshold. Either end restores a peer only when a packet from it also acknowledges something
new, that is, when the path works in both directions. A one-way outage therefore raises one
`PeerInterrupted` / `PeerRestored` pair, not one pair per probe.

Set session and link timeout policies before hosting or connecting. They apply when the endpoint
is created; changing dedicated-server timeout settings requires a restart.

### Coming back: resume, takeover and flow control

A player who drops keeps their slot and `PeerId` while the server holds it (spec 77 § 7.4, ADR-8180). The reconnect
token from the last `Welcome` resumes it in every case where the player is still known to the server:

- after the server timed the link out (`grace`, 60 s by default);
- before the server noticed anything: a client that reconnects at once from a new socket (one-way loss, a radio that
  died, a restarted process) takes its live `loading`, `joining` or `in_game` slot over. The old link is dropped
  without a goodbye, `PlayerResumed` fires and the player reloads the level. On a full server the returning player
  is never refused `server_full` by its own old slot. A token that matches nothing raises `TokenRefused { link }`.

To reconnect on your own, keep the session and call `reset()` then `connect_on(socket, server, ConnectOptions {
resume: true, .. })`. After a platform suspend, `NetSession::resume()` does it for you over the transport the session
connected with: lobby clients over the lobby's transport (Steam sockets), `connect` over UDP. For your own socket kind
(QUIC, a relay) connect with `connect_via(opener, server, opts)`, where `opener` is a `SocketOpener` that opens a fresh
socket; `set_socket_opener` names it after a plain `connect_on`. A plain `connect_on` with no opener cannot be
rebuilt: `resume()` then returns a transport error and keeps the session suspended, and you reconnect it with
`resume_on(socket)`.

A full reliable window is flow control, not a disconnect. The session holds what the transport refused, pauses
streams while the window is half full or the peer is interrupted, and `SessionServer::try_send_game` returns the bytes
for you to retry later (`is_backpressured(peer)` tells you beforehand). Only a backlog past 8 MiB ends the link, and
if the link was silent at that moment the player keeps the grace slot. When a phone's NAT moves an encrypted peer to a
new address, the session follows it, so `ban_player` bans the new address.

Address changes are validated before they take effect (ADR-8299). When a player's records start arriving from a new
address (a NAT rebinding, Wi-Fi to cellular, a carrier-grade NAT), the server keeps sending to the old address, sends one
small probe to the new one, and moves the player when the next record from there acknowledges it: about one round trip,
with nothing lost (reliable messages resend, and the new address's records are delivered meanwhile). Someone who copies a
player's traffic and races it from another address cannot pull the player's downstream away; the server sends that address
at most three times what it received from it. A player moving into an address block that already holds
`max_peers_per_ip` players keeps its slot: the cap limits joins, not moves. Clients follow a server whose address changes
the same way. `net.peers` shows per player `rebinds`, `rebinds_refused`, `path_probes`, `path_mirrored` and `path_candidate`.
On a plain (unencrypted) link a packet from the new address is held until the probe is answered and then applied, since
nothing proves it is the player's before that (ADR-10077); encrypted links deliver it at once.
While the old address has gone silent and the new one sends the player's newest records, the server also copies its traffic
to the new address (within the same three-times limit), so a player whose NAT rebinds keeps receiving while the move
validates; a move that changes only the port keeps the round-trip estimate (ADR-8422).



### Coming back in a replication game (`NetLink`)

A game hosted through `NetLink` (the `net-game` template, `HostedLink`) keeps a dropped player the same way (spec 120
§ 10.3, ADR-8298). When a player's connection times out, the server does not remove it: it is **dormant** for
`LinkOptions::resume_grace_ms` (60 s by default; `0` turns this off). Its `PeerId`, its entities and their ownership stay;
its entities carry the `NetInterrupted` marker on the server, and other players see it too when every process
calls `NetPlugin::replicate_interrupted_marker` (show "reconnecting", freeze the pawn). The
server reports `LinkEvent::Dormant { peer }` instead of `Left`, so do not despawn the avatar there.

On the client, call `NetLink::resume()` when the network is back (a new UDP socket, as a phone gets after it lost signal),
or `resume_on(socket)` for your own socket kind. The link presents the resume ticket of its last accept (single use, a new
one on every accept) and comes back as the same peer: the server reports `LinkEvent::Resumed { peer, .. }`, the client's
`LocalPeer` is unchanged, and the first frame is the full state, so local drift is corrected and entities the server
despawned meanwhile go. A ticket presented while the server still holds the old connection (the phone came back before
the timeout from a new address) takes that connection over at once.

If the grace ends first, the server reports `LinkEvent::Left { reason: "timeout" }`. To choose what happens to the
player's entities, insert a hook in the server app; it runs once per expired player, before the leave event:

```rust,wrap
# use regia::prelude::*;
use regia_net::{GraceVerdict, NetGraceHook, PeerId};

fn on_grace_expired(_world: &mut World, _peer: PeerId) -> GraceVerdict {
    GraceVerdict::Despawn // or Keep: the server takes the entities over
}
# fn setup(app: &mut App) {
app.insert_resource(NetGraceHook::new(on_grace_expired));
# }
```

Without a hook nothing changes in the world and your `Left` handler decides, as before. Reliable messages sent to a
dormant player wait for it (those older than the grace are dropped on its return); unreliable ones are dropped.
`net.resume` shows the dormant players and the counters.

### Reconnecting by itself

You rarely call `resume` yourself: a client whose server went silent past the timeout reconnects on its own (spec 120
§ 10.3, ADR-8352). It starts a fresh handshake with its resume ticket 1 s after the timeout, then 2 s, 4 s and every 8 s
(less a small random jitter, so a server that lost many players at once is not hit by all of them together), for the
server's 60 s grace. Each attempt keeps resending its handshake until the next one starts, so when the network comes back
the player is in again within a second or so. The attempts go over the transport the client connected with: `connect_udp`
opens a fresh UDP port each time, `connect(socket)` (a Steam or QUIC socket) reuses its socket, and
`connect_via(opener, ..)` opens a fresh socket of your kind each time. With an opener, a socket that keeps failing while
the server is quiet (a phone's Wi-Fi socket after it switched to cellular) reconnects at once instead of waiting for the
timeout.

The client's events tell the player what is going on, once per outage:

| Event | Show |
|---|---|
| `LinkEvent::PeerInterrupted { .. }` | "Connection interrupted" (the game keeps running) |
| `LinkEvent::Reconnecting { attempt }` | "Reconnecting (attempt N)" |
| `LinkEvent::PeerRestored { .. }` | nothing: back to normal, same `PeerId` |
| `LinkEvent::ReconnectFailed { reason, .. }`, then `LinkEvent::Ended { .. }` | "Disconnected", back to the menu |

The template's `LinkBanner` and its `link_banner` HUD node do exactly this. `NetLink::is_reconnecting()` and
`reconnect_status()` (attempt, time to the next one, window left) are there for your own UI; `net.status` shows the same
under `reconnect`. Set `LinkOptions::reconnect: ReconnectPolicy::off()` to end at the timeout as before.

A `NetSession` client does the same with its reconnect token: `ClientSessionEvent::Reconnecting { attempt }`, then the new
`Welcomed` and `PeerRestored`, or `ReconnectFailed` and `Disconnected`. It reconnects over its `SocketOpener` (a lobby's
Steam sockets), UDP, or the socket you gave `connect_on`, for `reconnect_grace_s`. `set_reconnect_policy` changes or turns
it off; a suspend or a quit cancels it.

## Handshake port changes and QUIC packet budgets

A client whose NAT changes its source port after Challenge can finish joining
with the same cookie, including on password-protected hosts. Cookies and password
nonces bind the exact canonical IP, client nonce and time bucket. A changed IP
still needs a fresh challenge. Before a valid key reply, a later challenge for the
same nonce replaces an unusable one; repeated challenges do not extend the timeout.
An invalid-cookie answer receives a fresh challenge within the server's existing
pre-proof answer budget, without allocating a peer or doing a key exchange.

The client keeps the first key exchange it started when a later challenge replaces it
(ADR-7995). On a path whose round trip is longer than the request resend, several
requests are answered, and when the cookie's 10-second time bucket turns over between
them the answers differ. The server stores the first key share it sees, so the client
keeps asking for both and finishes on whichever the server answers. A NAT port change
after the key reply also completes: the server finds the half-open join by address and
session token and moves it only after the sealed join verified. A plain join answer
replayed from another port of the same address makes no second peer. Relay registration
cookies bind the exact source address and port.

The optional loopback QUIC socket starts at 1200 bytes with path discovery enabled.
Regia reserves QUIC framing space: its connection packets use at most 1156 bytes,
or the peer's smaller payload limit. Reliable messages are fragmented to that
budget; unreliable messages must fit it. A size refusal reschedules uncommitted
reliable work without consuming a packet sequence. Custom `DatagramSocket` wrappers
should forward `max_datagram_size(dest)`; implementations with a smaller budget
should override the default. Previously transmitted reliable fragment IDs cannot
be reassigned, so transports must preserve their minimum payload budget.

Oversized QUIC receives are counted and skipped. Use `net.quic_status` to read
`datagrams_lost` and `last_error` (`quic_bind` for size problems), and `net.status`
to see whether the client is connected. See [QUIC datagrams (agents)](../agents/net-quic.md)
and [Networking (agents)](../agents/net.md) for the verification commands (ADR-7946).

### Loss, pacing and recovery (ADR-7995)

The v1 packet header acknowledges its newest packet and the 32 before it. The three lanes that
touched the send loop (ADR-7946, ADR-7949 with ADR-7988, ADR-7950) now share one policy, with
the wire format unchanged:

- **Acknowledgment ranges.** When more than 33 packets arrived since the last reply (a burst,
  or a receiver that stalled), the reply names every older range of 33 in header-only packets,
  oldest first and each twice, before the regular packet names the newest. A delivered burst
  is acknowledged in full, even when one of those datagrams is lost; a sender is no longer
  held to what one reply names. A late packet is reported anywhere in the 1,024-packet window.
  A peer on v0.5.0 or v0.5.1 sends one range per reply: bursts above 33 packets per reply are
  repeated toward it as before.
- **Loss needs evidence.** A packet is lost when a packet sent three or more later was
  acknowledged, or when a later packet was acknowledged and it has been out for 9/8 of the
  round trip. Silence alone is never loss, so a 3-second path counts none. A packet
  acknowledged after all is taken back out of `packets_lost` and `loss_fraction`, and both
  thresholds widen to the reordering that path showed. Reordering is not loss.
- **Resends.** A fragment whose packet was lost is sent again at once, but at most once per
  round trip, and one poll sends at most 8 packets of resent data (fewer on a long round trip,
  see "Repairs on a long round trip" below). A fragment with no verdict
  is resent by its timer: the retransmission timeout, never shorter than 9/8 of the measured
  round trip (so `LinkConfig::resend_max_ms` no longer forces resends on a path slower than
  it), with the backoff above. Timer resends are probes: 4 packets per poll, 32 per second.
  Round-trip samples come only from the newest packet a reply names, never from one that was
  declared lost or carried a resent fragment; a sample far above the estimate (a peer that
  stalled) is smoothed in only after a packet sent later confirms it.
- **Data lost again and again** (ADR-10088). The second repair of the same fragment, and every
  later one, leaves up to 7/32 of a round trip later than one round trip (a different share
  each time). The repairs of one burst then leave apart instead of back to back every round,
  and a repair does not meet a queue at the instant that dropped the last one. Data lost once
  is repaired exactly as before, and a fragment still leaves at most once per round trip.
- **A repair leads its poll** (ADR-10125). A full fragment fills a packet. When one is resent,
  it is the first packet of its poll, and the tick's credit and essential records (inputs,
  tick sync, state acknowledgments) ride the second packet of the same poll instead of the
  first. Datagrams of one poll reach a bottleneck back to back, and a queue that is full takes
  the first and drops the ones behind it; before, a 40-byte essential record put the repair
  behind a packet of state, where such a queue dropped it poll after poll. New data does not
  lead, and a link configured for one packet per poll keeps it for the essential records.
- **Repairs on a long round trip** (ADR-10125). The repairs of one poll are spread over a
  quarter of the round trip: a poll sends 8 packets of resent data divided by the number of
  ticks in a quarter of the round trip (`Connection::repair_packets_per_poll`). Under 8 ticks
  of round trip that is all 8, as before; from 20 ticks on (0.67 s at 30 Hz, a standing queue)
  it is one repair packet per poll, each first in its own poll, and polls less than half a
  tick apart share one budget. While repairs leave one per poll, ordinary unreliable data
  (state frames, voice) waits for them, for at most a quarter of the round trip at a time: the
  poll before a repair and the polls that carry one send no state, so the connection's own
  state packets do not take the room at the bottleneck that the repair needs. The state of a
  tick that waited is dropped as stale and counted in `unreliable_dropped`; the next tick
  sends a newer one. Essential records are never held.
- **Half fragments** (ADR-10189). A fragment can only be resent whole, and a full fragment is
  a full packet: behind a bottleneck queue that counts bytes and is kept full, the largest
  packet is the one that finds no room. A reliable message of more than one fragment that is
  queued before the path is measured, or on a path whose round trip is 8 ticks or more
  (`Connection::cuts_small_fragments`), is therefore cut into half fragments, two to a packet.
  The first flight is the same packets as before (five bytes longer each); a lost fragment is
  repaired in half a packet, and where repairs leave one per poll that packet carries the one
  half fragment and nothing else; the tick's records ride the packet behind it, and state does
  not wait for such a repair while the pacing allowance carries both packets (it does for a
  full fragment's, above, and for a half fragment's on an allowance smaller than the state). A measured short path cuts full fragments, and so does a
  stream that already holds a quarter of `max_reliable_in_flight` in queued fragments (half
  fragments use twice the ids; `send` accepts and refuses exactly the messages it did). A
  message that fits one fragment is never cut. Nothing changes on the wire: the receiver has
  always taken fragments of any length.
- **In-flight window.** Reliable bytes sent and neither acknowledged nor declared lost are
  bounded by a window that follows delivery: twice the bytes acknowledged within one round
  trip. It starts at `max_packets_per_poll` packets (64 by default, never under 16), doubles
  each round trip while the path keeps up, and shrinks only when three window-limited round
  trips in a row delivered less than half of it. Random loss does not shrink it. A peer that
  acknowledges almost nothing is held to a few packets per round trip. Unreliable data
  (inputs, state frames, voice) is not held by it.
- **Outage recovery** as described under "Silence, interruption and outage recovery".

`LinkStats` reports the result: `packets_lost` and `loss_fraction` follow the loss the link
really had, `reliable_resends` counts repeats for either reason, `recoveries` counts outages,
and `transport_stalls` counts polls that could send nothing because the transport's datagram
budget was under the 256 bytes the protocol needs. On a path whose first round trip is longer
than `resend_min_ms`, a few packets are repeated before the first acknowledgment arrives.
On a path with a multi-second round trip and heavy loss, a lost reliable message now waits
about one round trip for its repair where v0.5.0 repeated it every second; unreliable data
and inputs are not affected.

### One-round-trip resume (wire 6, ADR-8858, ADR-8902)

Wire 6 opens with the pieces of a faster comeback. On a sealed link the server's accept carries a second, larger ticket
(the resumption ticket) next to the 16-byte one, and `NetClient::resumption()` returns it with the link's secret as a
`ResumeCredential`. Pass it as `ClientConfig::resumption`, with `ClientConfig::resume` as before, and the client opens
its next connect with one datagram that carries the ticket, the proof and a new key share, instead of asking for a cookie
first. A server that admits it answers once and the client is connected: one round trip instead of three, which is what
gets a rejoin over a path with a full second of queueing inside a three-second grace.

The server admits the resume when the client's first packet under the new keys arrives (ADR-8902), not when the
first datagram does. That order is what makes the shortcut safe: someone who recorded your first datagram can send it
again as often as they like, from your address or another, and nothing happens to your player, your slot or your
ticket. A player whose connection timed out, or whose old connection the server still holds, comes back as the same
player. A player who left on purpose comes back from the same address as a new player in the slot that address kept.
From a new network (Wi-Fi to cellular) the server asks for one extra exchange first: two round trips instead of one.
A ticket works once; after that the next accept's ticket is the one to keep.

When the server cannot resume (the ticket is too old, the server restarted with another key, the server is busy), it
answers the first datagram the way it answers a connect request, and the client carries on with the ordinary handshake
from there, in the same three round trips as before. A build on wire 5 and a build on wire 6 refuse each other at the
handshake with `protocol_mismatch`, and each tells its player which side is older.

You do not pass the credential yourself unless you drive `NetClient` directly (ADR-10088). A `NetLink` client keeps
the credential of its last accept beside the ticket and presents both on every reconnect: `resume()`, `resume_on(socket)`
and the automatic reconnect, on its own socket or on fresh ones (UDP, a relay, Steam). A `NetSession` client does the
same on every connect with `resume: true`: the automatic reconnect, `resume()`, `resume_on(socket)` and your own
`connect` calls, direct or by join code. Its host keeps the transport's ticket of a player that timed out for
`session.reconnect_grace_s`, so the player's link comes back in one round trip and the session's reconnect token then
resumes the player on it; a suspend and `resume()` come back the same way. Nothing changes in your code or in the
events you get. The reconnect is the full handshake, as before, on a plain link (no credential exists), with
`reconnect_grace_s` or `resume_grace_ms` at 0, and when the host refuses the resume. `NetSession::transport_totals()`
shows which one happened: `resumed_one_rtt` counts the one-round-trip resumes, `resume_fallbacks` the flights that went
on to the full handshake.

Over a path with a long standing queue the difference is the whole reconnect grace. Measured on the soak's
`asym-bufferbloat` profile (4 Mbit/s down behind a 1 s queue that cross traffic keeps full): a rejoin connects 1,040 ms
after the link is up where the full handshake took 3,104 to 3,376 ms; `docs/perf/net-resume-wire6.md` has the table for
every profile.

### Transport v2: packet numbers and acknowledgment frames (wire 5, ADR-8184)

From wire version 5 (spec 120 § 4.1) the policy above runs on a new packet layout; builds on
wire 4 or older cannot join and are refused with `protocol_mismatch` (their version probe is
answered, so the player is told which side to update).

- **Packet numbers.** Every packet carries the next 62-bit number and no number is used twice;
  a repaired fragment travels in a new packet, so an acknowledgment always says which copy
  arrived. Numbers are sent in 2 or 4 bytes.
- **Acknowledgment frames.** One frame names up to 32 runs of received packets plus how long
  the newest one waited, so one reply acknowledges a burst of any size (v0.5.x needed four
  packets for a 64-packet burst). Older runs that one frame cannot name leave first in frames
  of their own. A receiver acknowledges every second packet that carries data, within 10 ms
  otherwise, and at once when packets arrive out of order or a keepalive asks. Packets that
  only acknowledge are not acknowledged back.
  A frame stops naming packets once the peer has acknowledged a packet that carried them, so
  it stays a few bytes long however long the match runs or however many packets were lost
  earlier; `LinkStats::ack_frame_bytes` counts what frames cost (ADR-8423).
- **Probe timeout.** The per-fragment retransmission timers are gone. While reliable data
  waits and nothing has been sent for `smoothed round trip + 4 x variation + 10 ms` (within
  `resend_min_ms` and `resend_max_ms`), one or two probe packets carry the oldest waiting
  fragments; each consecutive expiry doubles the wait up to `resend_backoff_max_ms`. Keepalives
  are pings that the peer acknowledges, so during an outage the path is probed by keepalives
  and after the first probe nothing is repeated until it answers: an 8 s outage costs one
  resend of a waiting fragment (v0.5.x: five).
- **Round trips** come only from the newest packet a frame names, less the peer's reported
  wait (at most 10 ms). `LinkStats::min_rtt_ms` is the smallest round trip of the last 10 s.
- **Redundant copies.** A reliable message of up to 256 bytes (an RPC, a chat line, an order)
  rides once more in the next packet that leaves anyway, 1 to 20 ms after it, when that packet
  has room. At 5 % loss and 40 ms one-way, small reliable messages arrive within 44 ms at the
  95th percentile instead of about 130 ms; no extra datagram is sent, and bulk data is never
  copied. A repair of such a message (after a loss) takes one copy of its own the same way, and
  when nothing else is leaving the copy goes in a small packet of its own at the next poll, so a
  message lost near the end of a session needs another round trip only when both the repair and
  its copy are lost (ADR-8400). `LinkStats::redundant_copies` counts them (they are not
  `reliable_resends`).
- **Receive timestamps** (`LinkConfig::receive_timestamps`, off by default): the peer reports
  when it received up to 16 packets per frame; `Connection::take_receive_timestamps` returns
  them for a rate controller (spec 120 § 4.2).

New counters: `LinkStats::spurious_losses` (packets declared lost that arrived after all; each
is also taken back out of `packets_lost`), `probe_timeouts`, `ack_only_sent` and
`redundant_copies`.

### Path MTU discovery (opt-in, ADR-8300)

Packets are 1,200 bytes at most, every header and the encryption included: every path carries
that. Many paths carry more. With path MTU discovery on (spec 120 § 4.3), a connection finds out:

- **Turn it on** per server with `mtu_probing = true` under `[net]` in `server.toml` (an
  optional key; without it probing is off), or per link with
  `LinkOptions::mtu_probing` / `LinkConfig::mtu_probing`. It is off by default, and off changes
  nothing on the wire.
- **Probes.** Once the first round trip is measured the connection sends a ping padded to 1,280
  bytes, then 1,400, then 1,452. Only an acknowledged probe raises the packet size; three lost
  probes of one size end the search at the last size that worked (the next size is tried again
  after 10 minutes). Probes carry no game data, so losing one costs nothing.
- **Black holes.** Some paths stop carrying large datagrams without telling anyone (a VPN comes
  up, a route changes). The first lost packet above 1,200 bytes sends data at 1,200 bytes again
  from the next packet while a probe checks the size; three lost checks drop the size to 1,200
  for a minute. An outage is not a black hole: a probe nobody answers is sent again, not counted.
- **What gets bigger.** Unreliable messages (`Connection::max_unreliable_bytes` follows the
  probed size) and how much one packet carries. Reliable messages are still cut for 1,200-byte
  packets, so a fall back never leaves one too large to resend. To send larger replication state
  frames, raise `replication.mtu_bytes` in `config/net.ron` (up to 1,452): the transport caps it
  to the probed size every frame.
- **Watch it** in `LinkStats::path_mtu` (the confirmed size; it stays while a lost full-size
  packet has data at 1,200 for a round trip), `mtu_search` (`disabled`, `base`,
  `searching`, `search_complete`, `error`), `mtu_probes_sent`, `mtu_probes_lost` and
  `mtu_black_holes`.

- **Don't-fragment.** Every `UdpTransport` socket sets don't-fragment (ADR-8893), so a probe
  larger than the path is lost or refused, never carried in pieces and acknowledged by mistake. A
  probe the operating system itself refuses as too large ends the search at the size below.
  `net.socket_buffers` reports `dont_fragment`. On QUIC, `QuicListener::set_mtu_probing` and
  `QuicDatagramSocket::connect_with_mtu_probing` switch QUIC's own discovery (on by default).

On a path that carries 1,280 bytes the search settles at 1,280 and never sends a larger data
packet; when the path drops to 1,200 the connection is back at 1,200 within three probe losses
with no reliable message lost (`cargo test -p regia_net --test pmtud -- --nocapture`).

### Rate control and pacing (ADR-8301)

The transport measures each path and sends no faster than it carries (spec 120 § 4.2). It is
on by default; `config/net.ron` (format 3) configures it under `session.rate_control`.

- **What it reacts to.** A queue building on the path (the round trip grows over its minimum)
  cuts the rate to 85 % of what was delivered over the latest second (or the latest round trip,
  when that is higher; a listen server's clients still use the latest round trip alone). Random loss on a path with room, as on Wi-Fi,
  does not: small reliable messages ride twice instead. Loss of 10 % or more cuts only with a
  queue or when the path delivers less than the game sends, and the rate comes back within 2 s
  of the loss clearing. Without a queue such a cut is a test: when the loss stays after the cut
  (random loss, not the rate's fault) the rate goes back up within about six round trips; when
  it falls (a policer) the cut stands. The test is only made on a sender that filled its rate
  in two feedback rounds in a row, compares the loss after the cut with the path's own
  long-run loss, and runs until its packets can tell the two apart, so a thin stream (a few
  packets per round trip) on a path that loses 5 % at random keeps its rate (ADR-10157). On a
  jittery path the queue threshold rises with the measured jitter, so jitter is not mistaken
  for a queue.
- **Whose queue it is (wire 6).** Every acknowledgment carries the time its newest packet
  arrived, so each side also knows the delay of its own packets on the way out, apart from the
  way back. A side that sends less than half its rate (a client sending inputs, say) is cut for
  a queue only when its own packets met one: a busy or slow return path, or the timing of the
  other side's socket reads, lowers a rate that was not in use far less often. A side that fills
  its rate, and a listen server toward its clients, is read from round trips, as before.
  `RateController::unconfirmed_overuse` counts the rounds in which such a cut was held back.
- **A deep queue is drained (ADR-10145).** A cut that finds 100 ms of queue or more at the
  path's bottleneck, or such a queue standing for two round trips while the sender fills its
  rate, sends far under the path's rate for a moment (down to a quarter of it, for as long as
  the queue needs to empty, 1.5 s at most) and then returns to the rate the cut left. Before,
  the 85 % cut held a full buffer for seconds: every snapshot sent behind it was that much
  older. A queue the drain does not move belongs to other traffic and is left alone for 15 s.
  `RateController::draining()` and `drains()` show it.
- **Joins on thin links: the start rate.** A new connection starts at
  `start_bytes_per_second` (256 KiB/s) and its first burst is the initial window, so a join's
  world data reaches a slow downlink faster than it carries: on 256 kbit/s behind a 16 KiB
  buffer a 16 KiB join blob fills the buffer and the next 0.8 s of snapshots arrive late. If
  your players may be on such links, lower the start rate:
  `rate_control: (start_bytes_per_second: 32768)`. The first burst follows it (at most 300 ms
  of the start rate, eight packets at 32 KiB/s) and the rate doubles every round trip from
  there. Measured: the join blob then arrives about 0.2 s later on a good link, 0.34 s later
  across the Atlantic and 0.26 s earlier on the thin one, whose snapshots stay within 7 ticks.
- **Joins on thin links: the first flight (ADR-10156, off by default).** Instead of a lower
  start rate, let the path answer first: with `rate.first_flight = true` in the `LinkConfig`
  you give `ServerConfig::link` and `ClientConfig::link` (code only; there is no
  `config/net.ron` key yet), a new connection sends eight packets and then the essential
  floor's rate until its first acknowledgments have come back. A path that fell behind even
  that starts from the rate it delivered; any other starts at `start_bytes_per_second`, as
  without it. The thin link's snapshots stay within 7 to 8 ticks at the default start rate;
  the join blob arrives about 0.11 s later on a good link and 0.27 s later across the
  Atlantic. For that first round trip `send_allowance` reads the floor's rate. `RateController::first_flight()` is true
  while the flight lasts.
- **Idle links and outages.** A connection that has been quiet (only keepalives) keeps its
  start rate, so a download after a loading screen starts as fast as on a fresh connection. After
  an outage of a second or more the controller starts measuring afresh from its current rate; it
  does not count the outage against the path.
- **Pacing.** Packets leave spread out at 1.25x the rate instead of in one burst per tick; the
  first burst of a connection is still the initial window. A reliable message of up to four
  packets (a 4 KiB command) on a foreground stream leaves whole once the pacer lets it start, as
  long as the connection has been sending less than its rate; a connection that fills its rate,
  or one under a listen server's shared uplink, paces every packet.
- **The floor.** Inputs, tick sync, acknowledgments and keepalives always have their own
  16 KiB/s up and 32 KiB/s down, whatever the rate. A path that cannot carry even that at over
  50 % loss for 10 s marks the peer interrupted rather than flooding it.
- **Listen servers.** A listen server's clients share the host's home uplink; the server splits
  it fairly (spectators get a quarter share). A dedicated server can set a fixed uplink with
  `uplink_bytes_per_second`.
- **A shared uplink is counted on the wire.** The aggregate, each peer's fair share and
  `uplink_bytes_per_second` count what the host's link carries: each packet's body, its endpoint
  header (9 bytes, 34 when encrypted) and 28 bytes of IP and UDP headers
  (`cc::DATAGRAM_OVERHEAD_BYTES`; an IPv6 path costs 20 more per packet, which is not counted).
  Set `uplink_bytes_per_second` to the uplink's real rate divided by 8. A peer's own controller
  still steers packet bodies (its target and `rate_target_bps` read in body bytes);
  `Connection::wire_bytes_per_body_byte` converts.
- **What the game sees.** Replication sizes each frame to `min(client_rate_bytes, allowance)`.
  A game that sends its own traffic reads `NetServer::send_allowance(peer)` (bytes per second)
  or `LinkStats::send_allowance_bps` and lowers what it sends; `queue_delay_ms` shows the
  queueing delay. `NetServer::next_send_at_ms` says when the pacer lets the next waiting packet
  go, for hosts that flush more often than once per tick.
- **Turning it off.** `rate_control: (enabled: false)` (or `LinkConfig::rate.enabled = false`)
  sends every due packet at once, as v0.5 did.

`regia_net::congestion::LossNotches` (named `CongestionNotches` until v0.7) steps a budget on loss alone. It is not
congestion control: random loss lowers it on a link with spare bandwidth. Size sends by the allowance and use it, if
at all, only as a cap below that (for example fewer cosmetic entities under loss).

### Timely datagrams (ADR-7949)

Unpacked unreliable records stay queued until one tick after their first poll. Ordinary and
essential queues each hold 64 KiB including framing; either can return `SendError::Backpressure`.
`LinkStats::unreliable_dropped` counts deadline expiry. Inputs, tick sync and state acknowledgments
use the essential queue ahead of reliable data. Direct `Connection` users can call `send_essential`
and `set_tick_rate`; endpoints set their rate automatically. Default lifetime is 17 ms (60 Hz).

The client's RTT-derived clock lead leaves room for two input ticks within the default 64-tick
server buffer. If the server uses a custom `input_buffer_ticks`, call
`NetClient::set_server_input_buffer_ticks` with the same value; v1 does not negotiate it. This bounds
the RTT contribution, not elapsed real simulation time. `net.status` totals and `net.peers` rows
report `inputs_too_far_ahead` when received samples exceed the horizon; old inputs are not counted.

State frame assembly accounts for the actual connection and bridge framing even if
`replication.mtu_bytes` is larger than the transport cap. The bridge omits an oversized unreliable
frame, and the next state tick heals it; it never promotes a snapshot to ordered delivery. A single
oversized replication record still uses the explicit reliable-record path. A configured state MTU
below 208 fails parsing with `replication_mtu_too_small`, rather than silently growing its payload.

## Lockstep matches

For a game whose whole simulation is deterministic and too large to replicate (a strategy game, a large co-op
simulation), `regia_net::lockstep` runs the match in lockstep (spec 120 § 7, ADR-8146). Nothing about the world is sent:
one **host** decides one frame per tick, the list of every seat's input for that tick, and every participant steps its
own simulation from the same frames.

| Piece | What it does |
|---|---|
| `LockstepHost<G>` | Buffers the seats' inputs, builds one `LockstepFrame` per tick on its clock, sends frames, sizes the input delay, tallies the state hashes. A listen host (`HostRole::Listen(seat)`) is also a player; a dedicated host is an `Observer` (it steps its own simulation and has a vote) or a `Relay` (no simulation). |
| `LockstepSeat<G>` | Stamps its inputs, hands out frames in order, each once, reports its state hash. |
| `lockstep::net` | Carries both over `NetServer` / `NetClient`: implement `LockstepCarrier` for your `NetGame` to say how your message type wraps a lockstep payload, then `send_host`, `send_seat`, `host_receive`, `seat_receive`. The core itself is sans-IO: any transport with a datagram lane and a reliable ordered lane can carry it. |

The loop on a seat, once per frame of your game:

```text
for each ClientEvent::Message          -> lockstep::net::seat_receive(&mut seat, channel, &message, now_ms)
while let Some(frame) = seat.next_frame():
    step your simulation from frame.inputs (a seat listed Absent has no input this tick)
    seat.report_hash(frame.tick, your_state_hash)
    seat.submit_input(&sample_your_input())       // one input per stepped frame
lockstep::net::send_seat(&mut client, &mut seat, now_ms)
```

and on the host, once per tick: `host_receive` for each message, `while let Some(frame) = host.poll_frame(now_ms)`
(step the host's own simulation, `report_hash`, and for a listen host `local_input`), then `send_host`. The module's
rustdoc has a complete example that compiles.

What you get:

- **A lost packet does not stall the match.** Frames travel as datagrams; each one repeats the frames before the
  newest that the seat has not acknowledged, so the datagram after a lost one repairs it a tick later. A frame channel
  that resends on a timer stalls every seat for the whole timer instead. By default (`FrameRedundancy::Interleaved`)
  every datagram carries the newest frame and the one before it, and every second datagram reaches back over the
  loss bursts measured on that seat's link plus two: one lost datagram is repaired by the next, a longer burst by
  the first or the second datagram after it. A seat that still finds a hole says so and gets every unacknowledged
  frame. `Adaptive` reaches back over the link's loss bursts in every datagram (a tick sooner after a burst, for
  about a tenth more host upload at 40 seats); `AllUnacked` repeats every unacknowledged frame in every datagram,
  which costs more upload the longer the round trip is. Frames too large for a datagram go on the reliable stream,
  once, beginning at the last frame the seat acknowledged so that it can always apply them. A host that builds
  several frames between two sends (a hitch) still sends every one of them.
- **Small frames.** A seat whose input did not change costs one bit; a changed input costs the bytes that changed.
  Frames carry the exact bytes each seat sent, so every participant decodes the same value whatever your input codec
  does with a `-0.0` or a NaN.
- **An input delay that follows the link.** Inputs are stamped `capture tick + delay`. The host measures how late each
  seat's inputs arrive and how much loss delays them (how often the link loses and how long its bursts run: the
  budget covers all but one datagram in two hundred each way), raises the delay at once when a seat needs more, and
  lowers it by one tick after five calm two-second windows. Every seat switches on the same tick: the change rides in
  the frames. `LockstepConfig::max_delay_ticks` bounds it (15 ticks at 60 Hz).
- **A seat that sits on its inputs is flagged.** An honest client sends each input with the acknowledgement of the
  frame that opened its capture. The host measures the time between the two: a client that samples later than its
  capture tick, as late as the delay allows, shows as `HostEvent::LateStamp` and in `net.lockstep`
  (`late_stamp_suspects`). It is a flag for you and your operators; the engine removes nobody for it. Call
  `LockstepHost::note_transport_rtt(seat, rtt_ms)` once a second with `NetServer::peer_rows()`'s round trip so a
  client that holds its acknowledgements back is seen too.
- **No host advantage.** A listen host's own input is delayed like everyone's (`HostInput::Delayed`); choose
  `Immediate` if your game wants the host's input in the next frame. Either way `local_input` and `submit_input`
  answer `Stamp::Skipped` when the sample has no tick of its own (the delay was lowered, or under `Immediate` the
  next frame is one of the empty ticks of the start or of a delay raise): carry a press you must not lose into your
  next sample.
- **Desync detection that cannot be one peer's word.** Call `report_hash` after every step. At checkpoints (every 30
  ticks) the host tallies the hashes with its own as one vote. A strict majority decides: a seat in the minority is
  told to take the majority's state (`HostEvent::ResyncSeat`), the host in the minority is told the same
  (`HostEvent::ResyncHost`) and no seat is removed for it. A seat is removed with `desync` only after `max_resyncs`
  transfers. With two voters there is no majority: the tie removes nobody; by default everyone takes the state of the
  group the host votes in, and `TiePolicy::EndMatch` ends the match as `desync_unresolved` instead. Run a dedicated
  `Observer` host for competitive two-player matches: it is the third vote. If your game can replay from a snapshot,
  handle `SeatEvent::SelfCheck` / `HostEvent::SelfCheck`: a one-off fault then repairs itself without a transfer.
  On a dedicated `LockstepServer`, implement `LockstepObserver::loads_state` and `load_state` (your catch-up bytes) and
  hand the majority seat's `ProvideState` bytes to `LockstepServer::load_observer_state` (or return them from your
  `LockstepSeatLink::observer_state`): an outvoted observer then takes the majority's state and keeps voting instead of
  leaving the quorum for the match (ADR-8855).
- **Desync reports that name the subsystem.** Implement `regia_net::desync::DesyncHash` for your simulation (one
  `world_hash`, and up to 64 named `subsystem_hashes`: units, economy, pathing...) and call `record_state(tick, &sim)`
  after each step instead of `report_hash`; in a replay (a self-check, or after taking another simulation's state)
  call `record_replayed(tick, &sim)` for each replayed tick (a game that reports plain hashes calls
  `report_replayed_hash(tick, hash)` instead), so the next probe compares the history your simulation has now, not
  the one it replayed over (ADR-10147). Every participant keeps its last 240 ticks of rows
  locally. After a verdict the host finds each diverged simulation's first differing tick, pulls the rows at that
  tick and the one before it, and reports `HostEvent::DesyncProbed` with the first diverged subsystem and the last
  agreed tick (also `last_desync` in `net.lockstep`). One player who sends forged hashes is one vote: it is resynced,
  then removed with `desync`, and nobody else stops. To prove your game's detection, flip a subsystem in your step
  with `DesyncInject::new("economy", tick)` (`inject.flip(tick, name, value)`) in a test build: the next checkpoint
  is a verdict and the report names `economy`.

### Sessions: silent seats, leaves, late join and reconnect

(Spec 120 § 7.4, ADR-8212.)

- **A silent seat never holds the match.** The host builds every frame on its clock. A seat whose input is not there
  is listed `SeatInput::Absent`, or with the input `LockstepConfig::missing_input` gives it (`SeatInput::Held`):
  `Hold` repeats its last input, `RepeatAnalog` repeats it with the presses cleared (give the host and every seat the
  same `InputRules` with `set_input_rules`), `Empty` lists it absent. Step a held input like a sent one.
- **A press is never lost or doubled.** An input that arrives after its tick was built goes into the next frame, once.
  With `InputRules::merge_late` your game decides how: keep the presses of both, let the newer analog values win.
- **Leaving and ending agree on a tick.** `seat.leave()` asks the host; every participant sees the leave in the same
  frame (`SeatEvent::Left`, `HostEvent::SeatRemoved { reason: "left" }`). `host.end_match()` makes the next frame the
  last one (`frame.match_end`); nothing is built after it.
- **Silence is judged by the session's timeout policy** (`LockstepConfig::timeout`, the same `TimeoutPolicy` as the
  rest of `regia_net`): `HostEvent::SeatInterrupted` after 1.5 s (show "reconnecting"), `SeatRestored` when it is heard
  again, removal with `reason: "timeout"` after 20 s. A seat that has not been heard since it was added (still
  loading) is not removed before `start_grace_ms` (30 s). Nobody is removed for loss, lateness or a long round trip.
- **Late join and reconnect.** Give each player `host.resume_token(seat)` when they join. When one comes back (or a
  new player arrives), the host calls `host.resume(token)` to get the seat id back and
  `host.catch_up(seat, state_tick, &state)` with your game's serialized state after a recent tick (the last
  checkpoint is a good choice; the frame log holds `frame_log_ticks`, 10 s at 60 Hz). On the player's side create the
  seat with `LockstepSeat::catching_up(config, seat)`, load the state on `SeatEvent::StateReady` (`take_state`), then
  step the frames it hands out: at most `catchup_steps_per_frame` per frame of your game, so the catch-up runs faster
  than real time without a hitch. It joins (or resumes its seat) on an agreed tick once it is at the live edge
  (`SeatEvent::CaughtUp`, `HostEvent::SeatJoined`). A state older than the frame log is refused with
  `lockstep_catchup_gap`; take a newer one.
- **Coming back on the same connection's ticket (wire 6, ADR-10052).** Over `NetServer`, set
  `ServerConfig::resume_grace_ms` (for example 30 000) and keep a `lockstep::net::SeatPeers`: `bind(seat, peer)` when
  you seat a join, feed it every server event (`seats.on_event(&mut host, &event, now)`) and send with
  `send_host(&mut server, &mut host, |s| seats.peer_of(s), now)`. A player whose link drops becomes dormant
  (`SeatLink::Dormant`); when its client connects again with the ticket of its last accept (`ClientConfig::resume =
  client.resume_ticket()`), the server reports the same `PeerId` and `SeatPeers` resumes the seat in that pass
  (`SeatLink::Resumed`; `host.resume_seat(seat, now)` underneath). The seat's frames then leave right behind the accept,
  a round trip sooner than sending the token after a fresh join. Call `catch_up` as above when the seat was removed
  meanwhile or its process lost its state. `SeatLink::Refused` (the seat left or its window passed) and
  `SeatLink::Lost` (the peer left for good) unbind it. Keep `resume_token` for a player whose process restarted.

Settings (`LockstepConfig`; every participant must use the same values):

| Field | Default | Meaning |
|---|---|---|
| `tick_rate_hz` | 60 | Ticks per second. |
| `initial_delay_ticks`, `min_delay_ticks`, `max_delay_ticks` | 4, 1, 15 | The delay the match starts with and its range. `LockstepConfig::at_rate(hz)` keeps the maximum at 250 ms. |
| `host_input` | `Delayed` | How a listen host's own input is stamped. |
| `delay_policy` | `Shared` | `Shared`: the delay follows the seat that needs the most. `PerSeat`: it follows the median seat and a seat on a worse link stamps further ahead on its own. |
| `delay_window_ms`, `calm_windows_to_lower` | 2000, 5 | The calm window and how many in a row lower the delay by one tick. |
| `late_grace_ms` | 0 | How long the host holds a due frame for a live seat's missing input. |
| `redundancy` | `Interleaved` | How many frames a datagram repeats (above). |
| `datagram_budget_bytes` | 1000 | Largest lockstep datagram; larger frames go on the stream. |
| `max_input_bytes` | 1024 | Largest input a seat may send (each input goes to every seat). |
| `frame_log_ticks` | 600 | Frames both ends keep for a replay or a state transfer. |
| `hash_every`, `vote_deadline_ticks` | 30, 120 | Checkpoint spacing; when a tally closes without the missing votes (held to 24 checkpoint intervals). |
| `self_check`, `max_resyncs`, `tie_policy`, `max_tie_resyncs` | true, 2, `HostGroup`, 2 | The quorum rules above. |
| `missing_input` | `Empty` | What a seat without an input steps with: `Empty`, `RepeatAnalog`, `Hold`. |
| `timeout` | 1500, 20000, 30000 ms | `TimeoutPolicy`: interruption notice, removal, start grace for a seat not heard yet. |
| `catchup_steps_per_frame` | 8 | Frames a catching-up seat hands out per frame of your game. |
| `resume_window_ms` | 120000 | How long a seat removed for silence may come back with its token. |

Limits in this version: a seat that joins mid-match captures from the tick after its join frame; the state a
catch-up carries is your game's to produce (a `Relay` host has none: get it from a seat with your own game messages); under
`PerSeat` the ticks a
seat skips when it raises its own lead are counted as missing; a seat you remove and add again before the next frame
joins one frame after its leave, as a new seat. When the seat's previous holder has not acknowledged the frame that
lists its leave, `HostEvent::SeatHandover` hands you the frames it still lacks: send them to the connection that
held the seat with `lockstep::net::send_handover` (from then on `outgoing` addresses the new holder). A listen host adds its own seat with `add_seat`
like any other; `local_input` answers `lockstep_unknown_seat` without it. Your simulation must be deterministic: use
`regia_detsim::math` for transcendentals and `regia_detsim::float` for min, max and clamp, and run
`cargo xtask det-scan` over it. Check a running match with [`net.lockstep`](../agents/net-lockstep.md).

### Desync bundles

When the probe names a subsystem, write a **desync bundle** so the divergence can be replayed offline, on any machine
with the same build (spec 120 § 7.6, ADR-8455). On `HostEvent::DesyncProbed`:

```text
let snapshot = BundleSnapshot::at(last_agreed_tick, your_state_bytes_at(last_agreed_tick));
let path = DesyncBundle::from_host(&host, snapshot)?.write_to_dir(&project.join("saved/desync"))?;
```

`last_agreed_tick` is the verdict's `last_agreed` checkpoint (`HostEvent::Desync`); keep your encoded state at each
checkpoint for it (the resync path needs the same state). A diverged seat can write its own bundle on
`SeatEvent::Resync`, before it takes the majority's state: `DesyncBundle::from_seat(&seat, snapshot)` with its state at
the `from` of the `SeatEvent::SelfCheck` before it. The file (`<game>-host-<checkpoint>.desync.ron`, format
`docs/formats/desync-ron.md`) holds the snapshot, every frame after it (each seat's input bytes), the probe's subsystem
rows and the live verdict.

To replay it, implement `regia_net::desync::DesyncReplay` for your simulation (the same step and the same
`DesyncHash` as the live match, plus `identity`, `load` for a snapshot and `step` for one frame; decode a frame's
inputs with `frame.decode::<YourInput>()`), register it, and replay:

```text
let mut registry = ReplayRegistry::new();
registry.register(YourGame::GAME_ID, || Box::new(YourSim::default()));
let verdict = registry.replay(&DesyncBundle::read(path)?)?;
for line in verdict.summary() { println!("{line}"); }
```

The replay steps seats in ascending order and compares each peer's rows with its own, peers in ascending order, so it
names the first tick and subsystem where a peer left the deterministic path, and says whether that is what the live
probe said. `regia net desync-replay <bundle>` does the same with the hooks of the `regia` binary (the engine's sample,
`regia.desync_sample`); call the same code from your game's own tool binary to replay your bundles. A bundle from
another game, or another version of it (protocol version, content hash or wire version), is refused with a message
naming both versions: replay it with the build that wrote it. In a development build, an agent plants a divergence with
[`net.desync_inject`](../agents/net-desync.md) and reads the verdict, the rows, the bundle path and the replay with
`net.desync`.

### Host migration: the match goes on when the host quits

In a lockstep match every seat holds the whole world, so the match does not have to end when the host's process quits
or crashes (spec 120 § 10.5). To turn it on:

1. Build a `MigrationRoster` with `MigrationRoster::generate(match_id, port, host_seat)`.
2. Add a `RosterSeat` for each seat: its Noise static public key (the `ServerIdentity` it would host with) and its
   address.
3. Call `host.share_roster(roster)` once the seats are added. It goes over each seat's sealed link.
4. Give every seat a `HostMigration::new(seat, MigrationConfig::default(), config)`.
5. Each application frame, call `migration.poll(&mut seat, now_ms)` and pass every migration message to
   `migration.on_message`. Do what `poll_action` asks: open the re-host listener, connect with the winner's key pinned,
   send messages on that link's reliable stream. Show what `poll_event` says: "Host lost", "Connecting to <winner>",
   "Game resuming in N".

The winner gets `MigrationEvent::Promoted`. It takes the new host with `take_host()` and keeps its own seat on an
in-process link. Seats that lacked frames get them from it; nobody steps back a tick. Feed the host's round trips with
`note_transport_rtt` so the best connected seat wins. Call `seat.note_host_heard(now_ms)` when the transport hears the
host, so a host whose frames are delayed is not mistaken for a lost one. A player who dropped, the old host included,
comes back with its resume token through `resume` and `catch_up` on the new host. See
[Net lockstep (agents)](../agents/net-lockstep.md#host-migration).

### Rollback lockstep for small action matches

For a fighting or sports game with 2 to 8 players, set `LockstepConfig::rollback(60)` (or `mode:
LockstepMode::Rollback { max_rollback_ticks }` on your own settings) on the host and every seat, and drive each seat
with a `regia_net::lockstep::RollbackRunner` instead of stepping `next_frame` yourself. Implement
`RollbackGame` for your state (`step(&LockstepFrame)` and `hash()`, the same step a strict seat runs), create the
runner with the state before tick 0, and once per application frame call
`runner.advance(&mut seat, now_ms, |capture_tick| sample_input(), |tick, confirmed_state| {})` between `receive` and
`outgoing`. Draw `runner.state()`: your own input shows after the configured delay (1 to 3 ticks), the other players'
inputs are predicted (their last input with presses cleared, by your `InputRules`) and corrected when their real
input arrives, never more than the window (8 ticks) back. The match state that counts is `runner.confirmed_state()`;
only it is hashed and voted on. After a state transfer or a catch-up call `runner.reset(next_tick, state)`. Set the same
`InputRules` on the host and the seats. A player whose round trip is longer than the delay plus the window covers is not
waited for: the host builds frames without holding them for that seat and merges its late inputs, so only that player
feels the bad link (`net.lockstep` `seats[].hold_released`). Set `LockstepConfig::release_slow_seat_hold = false` if
every player should wait for the slowest instead. See [Net lockstep (agents)](../agents/net-lockstep.md#rollback-lockstep).

## Predicting a step the game owns (spec 120 § 9.5)

When the game, not the ECS predictor, owns the simulation step, keep a `regia_net::RollbackLog`. Each tick, record the state after the step and the inputs that produced it. When the server's state for an older tick arrives, `reconcile` writes that state and replays the game's step through the newer ticks with the inputs the log stored. A correction older than the ring returns `rollback_too_old` and leaves the stored snapshots alone. `amend_inputs` drops a late input onto a past tick so the next replay uses it.

`VisualOffset` spreads the visual jump across a fixed number of ticks (an integer delta that reaches zero after that many ticks, with no clock). It is separate from the ECS predictor's smoothing. Publish `log.counters()` with `NetAgentState::publish_rollback` and read them with `net.rollback` (`replays`, `depth`, `too_old`). See [Net rollback log (agents)](../agents/net-rollback.md).


## Self-hosting a lobby hub

A lobby hub gives your game lobbies, a player directory with presence, and invites without Steam or any platform account
(`regia_net::lobby::hub`, ADR-8559). You run the hub; players connect to it with `HubLobbyBackend`, which is a
`LobbyBackend` like the Steam and local ones, so `Lobbies`, `Matchmaker` and `lobby.*` work unchanged.

The hub process:

<!-- ignore-reason: a fragment of the hub process's `main`; the identity and addresses are the operator's -->
```rust,ignore
use regia_net::crypto::ServerIdentity;
use regia_net::lobby::hub::{HubHost, HubSettings};
use regia_net::UdpTransport;

// Keep the identity across restarts (`ServerIdentity::to_file_text`, a private file): players pin its key.
let identity = ServerIdentity::generate()?;
println!("hub key: {}", identity.public());
let socket = UdpTransport::bind("0.0.0.0:7810".parse()?)?;
let mut hub = HubHost::new(socket, identity, HubSettings::default());
loop {
    hub.update(now_ms()); // receive, apply, expire, flush the directory, send
    // sleep a few milliseconds
}
```

The game:

<!-- ignore-reason: a fragment of the game's setup that returns errors with `?` -->
```rust,ignore
use regia_net::crypto::PlayerIdentity;
use regia_net::lobby::hub::{HubClientConfig, HubLobbyBackend};

let backend = HubLobbyBackend::connect(
    UdpTransport::bind("0.0.0.0:0".parse()?)?,
    "hub.example.org:7810".parse()?,
    HubClientConfig {
        handle: "ann".into(),
        server_key: Some(hub_key), // shipped with the game or given by the hub's operator
        // One player key across runs (friends lists and invites name it); omit for a fresh key
        // per connection. The file is a private `server-key` format file in the user's data dir.
        identity: Some(PlayerIdentity::load_or_create(&user_data_dir.join("player.key"))?),
        ..HubClientConfig::default()
    },
)?;
// Use it like any LobbyBackend: create, request_list, join, set_server, send_chat, poll.
// Hub extras: invite(lobby, "bob"), invites(), set_presence, set_friends, send_global_chat.
```

What to know:

- **The key is required.** `connect` refuses to start without the hub's key (`hub_key_unpinned`), and a hub that answers
  under another key ends the link with `hub_key_mismatch`. The link is always encrypted, loopback included.
- **Addresses stay private.** The hub learns where a game session runs only when the lobby owner calls `set_server`, and
  it tells the lobby's members only. Listings and invites never contain an address; an invite is a lobby id plus a token
  that only the invited player can use, for 2 hours.
- **Visibility.** Public lobbies are listed; invisible ones are joinable by id; friends-only lobbies take the owner's
  friends or invited players; private lobbies take invited players only. A friend is a player key, not a handle:
  `set_friends(&[key])` with the key each friend's `HubLobbyBackend::identity()` gives (share it like a friend code).
  Handles are display names; a player whose hello proved no key never enters a friends-only lobby.
- **If the host leaves.** The next member owns the lobby and the game address the old host confirmed is withdrawn: the
  new owner confirms its own (`set_server`) before anyone is sent one.
- **Fair use.** Data writes (20 a second per player, 160 per lobby), global chat (16 a second for the whole hub) and the
  hub's sends (1024 a second per player) are limited; a client that falls behind gets its view resent instead of being
  dropped, unless it stops reading. The resend includes the chat lines it missed and a join plan it missed.
- **Private stays private.** A join of a private or friends-only lobby you may not enter answers `lobby_not_found`,
  as for a lobby that does not exist; the hub's `lobby.hub_status` still counts the real reason.
- **Limits are settings.** `HubSettings` holds every cap (sessions, lobbies, chat rate, invite lifetime, idle timeout, data
  sizes) with safe defaults.
- **Watch it.** Publish `HubHost::publish("hub", &mut agent_state)` and call `lobby.hub_status`
  ([Lobby hub status](../agents/net-lobby-hub.md)) for sessions, games, generations and memory.

Join brokering (port mapping, hole punching, relay fallback) is `lobby::hub::join` (NL-05). The packaged process is
`regia-lobby-hub`: every `regia package --server` stages it with `hub.toml` and a systemd unit that starts it (NL-06,
ADR-8891). Your game still
connects to the address the owner confirmed, as with any lobby.

### Capturing a client (spec 120 § 15.5)

When one player lags and nobody else does, record that player's client: `let tap = regia_net::capture::CaptureTap::new(16 << 20)?;
client.set_capture(Some(tap.clone()));` logs every packet it received (after decryption), when it read and flushed, the input it
stamped, and, if your game calls `tap.frame(now_ms, digest)` once a frame, a digest of what it showed. Memory is the ring you ask for,
allocated once; `tap.save(path, label, false)` writes a `.netcapture`. `NetClient::replay` feeds a captured session into a fresh client
at the same milliseconds, so you can step through the exact corrections that player saw; `capture::compare_sessions` names the first
record where the replay differs. A capture holds decrypted traffic: treat it as private game data. The net soak writes one for every
client of a failing run, see [Net soak (agents)](../agents/net-soak.md).

A `NetLink` game does the same through its link: `link.set_capture(Some(tap))` (it follows the client across reconnects
and resumes), `link.capture_frame(digest)` once a frame with a digest of what the frame showed, and
`link.replay::<MyGame>(feed, &opts)` in a fresh world with the same setup and link options replays the session through
the whole client: bridge, replication, prediction and proxy interpolation, at the captured link-clock times. An agent
starts and saves a capture on a running game with `net.capture_start` and `net.capture_save`; see
[Net client capture (agents)](../agents/net-capture.md).

### Match replays (`.lsreplay`)

To keep a whole match and check it bit for bit later, record a `.lsreplay` (ADR-8513) beside your step, on the host
(or a seat). Recording is opt-in:

```text
let settings = LsReplaySettings { record: true, ..LsReplaySettings::default() };
let header = LsReplayHeader::of::<YourGame>(seats, tick_rate, seed, &settings);
let path = ReplayLibrary::new(&replay_dir).new_recording_path("match");
let mut writer = LsReplayWriter::new(BufWriter::new(File::create(&path)?), header, settings)?;
writer.record_start(&sim, |out| sim.encode_state(out))?;
// per frame:
writer.record_frame(&frame)?;
sim.step(&frame);
writer.record_state(frame.tick, &sim, |out| sim.encode_state(out))?;
// at the end (Some(...) when the match aborted):
writer.finish(None)?;
ReplayLibrary::new(&replay_dir).rotate()?; // keep the newest 20
```

`regia_net::lockstep::replay::audit_with(&bytes, &registry)` (the same `DesyncReplay` hook as the desync bundle) re-runs
the file from its Fulls and compares every recorded hash; `regia net replay-audit <file>` does it with the hooks of the
`regia` binary. Set `hash_every_ticks: 1` to name the exact tick a divergence starts at. The library lists, rotates,
imports (refusing a duplicate by SHA-256) and exports replays; agents use `net.replays` and `net.replay_audit`
([Net lockstep replay (agents)](../agents/net-lsreplay.md)).

### Connection quality (ADR-8182)

`NetClient::quality()` and `NetServer::peer_quality(peer)` return a `PeerQuality`: round-trip time with its p50/p95
over the last 10 s and RTTVAR, one-way interarrival jitter (RFC 3550), loss on what this end sent (`loss_out` from
acknowledgments, `loss_in` as the peer measured it: the server reports a client's upstream loss in its input feedback)
and on what it received (`loss_recv`), the input margin and the late and missing input fractions, and a `LinkGrade`:

| Grade | RTT p95 | Loss | Missing inputs |
|---|---|---|---|
| `Good` | < 150 ms | < 2 % | < 0.5 % |
| `Fair` | < 300 ms | < 5 % | < 2 % |
| `Poor` | otherwise | | |
| `Interrupted` | silent past `interrupt_ms` | | |

A worse grade shows after 500 ms, a better one after 2 s with every signal under 75 % of that grade's limits, so the HUD
does not flicker; a silent link moves no grade until it is `Interrupted`. Show the icon from `ClientEvent::QualityChanged { from, to }` (and `ServerEvent::QualityChanged
{ peer, from, to }` for a host's scoreboard); tell the player which way the trouble is from `loss_in` (their upload) and
`loss_recv` (their download). `NetSession::link_quality()` and `NetSession::peer_quality()` give the same for session
games. A dedicated server writes `regia_net::stats` tracing rows (each peer every 10 s, the server every 60 s: tick and
loop p50/p95/p99, resident memory, peers by grade) with addresses redacted; clients write none. A resumed peer, or a
`NetLink` client that reconnected by itself, gets a `QualityChanged` back from `Interrupted` to its new connection's grade,
so a HUD never stays on the outage (ADR-8889). Agents read it with
[`net.quality`](../agents/net-quality.md).

## Warm successors for a listen server (spec 120 § 10.5)

When the player who hosts a replication game quits, the match ends unless another player's process already holds the
world. `regia_net::migration` keeps one or two clients ready: the host picks its best connected clients as
**successors** and streams each a journal on the bulk stream. A successor applies it to a shadow world it can promote.
This is the first part of host migration for replication games; the second part, the hand-off, is the next section.

The game supplies three things:

- A full state: the bytes of its authoritative world, and a digest of that world.
- A log per tick: the inputs it stepped the tick with and the events of that tick, as two blobs. Journal what changes
  the world and is not in the inputs (joins, spawns, the host's random draws). Journal an input only when it changes.
- A `ShadowWorld` on each client: `load_full`, `apply` and `digest`.

On the host, after stepping `tick`:

```text
if journal.full_wanted(now_ms) {
    let digest = world.digest();
    journal.record_tick(tick, &inputs, &events, Some(digest))?;
    journal.record_full(now_ms, tick, world.encode(), digest)?;
} else {
    journal.record_tick(tick, &inputs, &events, None)?;
}
journal.pump_server(now_ms, &mut server, |bytes| MyMessage::Journal(bytes));
```

On every client, give journal payloads (`migration::is_journal_payload`) to a `JournalSuccessor` and send back what
`poll_ack` returns on the unreliable channel; on the host, give those to `JournalHost::on_payload`. A client the host
did not pick stays idle and costs nothing.

What to expect:

- A full state goes out every 5 s (`full_every_ms`), and the log every tick. The journal spends at most a quarter
  (`budget_share`) of what the rate controller allows for that client, so it never crowds out gameplay. An idle tick
  costs six bytes.
- Each tick the successor gets one small payload (at most 900 bytes: the log and a piece of the full state), and on a
  weak link the next one waits until the last was acknowledged. That keeps the journal from ever stalling the
  connection of the client it runs on.
- On a good link the shadow is one tick behind the host. A client whose link is near the rate controller's floor
  carries little journal: the host prefers other clients, and replaces a successor that fell out of the journal when
  a better link is there.
- If the log misses an event, the shadow notices at the next full state (`replay_divergences`), loads that full state
  and carries on. If a successor leaves, the second takes its place and the next best client is seeded.
- A successor holds at most 600 ticks of log after its newest full state. The host's log waits there for the next
  full state; a record past it would be refused.

Size the journal to the link: at the default share, a 64 KiB/s allowance carries about 16 KiB/s of journal, enough
for a 50 KiB world every 5 s next to a sparse log. One payload a tick is 54 KB/s at 60 Hz, whatever the allowance. Check a running match with
[`net.migration`](../agents/net-migration.md).

## Host migration: the hand-off (spec 120 § 10.5)

When the host of a listen match is lost, the first warm successor becomes the server and every other player carries on
there as the same peer. `regia_net::handoff` is that hand-off; it sits on top of the successor journal above.

What happens, with the default thresholds:

1. **While the match runs** the host tells every client where the match would go: the successors in order, each with
   the address it would serve on, the public key of the server it would become, and this client's ticket for it.
   Each successor gets a secret of its own and the list of players. All of it travels on the encrypted link.
2. **The host is lost**: a client has not heard it for 1.5 s (`host_lost_ms`), or the host said goodbye. The first
   successor **arms**: it opens a server under the key it announced, on the port it announced. Every player of the
   lost host is held there as a dormant peer under the `PeerId` it had. The successor still has its shadow world and
   its own connection to the host.
3. **Everybody else resumes.** Each client connects to the successor with its ticket and the successor's key pinned.
   It is accepted as the same peer, so its entities and their network ids are still its own. Only a process that holds
   the announced key can take the match over, and only a player of this match holds a ticket.
4. **The successor commits** when a majority of the players has arrived (or when its own connection to the host
   ended): it promotes its shadow world and the match runs again. If it hears the host again first, the silence was
   its own outage: it **disarms** and nothing happened.
5. **The first successor is gone too**: after 3 s (`takeover_ms`) without an answer the second successor arms and
   the clients resume to it.
6. **The lost host's own player** is held dormant under a new `PeerId` (the lowest id above every player's). A host
   that was only cut off sees every client go silent, stops hosting and comes back as a client on that seat.

A client keeps its connection to the host while it tries a successor, on a second socket. If the host is heard again
first (the silence was the client's own outage), the attempt is dropped and nothing changed for that client. A
successor in its own outage arms and disarms without anybody noticing. Either way there is still one match.

The world the match continues from is the successor's shadow: the lost host's world as of the last journal record that
arrived. On a good link that is one or two ticks before the loss; on a bad link it can be a second or more, and the
players see that much rolled back.

How long it takes, measured with 16 players on the soak harness's links (three seeds each, from the moment the host
stopped until the last client was connected to the successor again): 1.7 to 1.8 s on `good-wifi` and 2.1 to 2.3 s on
`bad-wifi`; with the first successor gone too, 4.7 to 4.8 s and 5.0 to 6.1 s. Most of it is the 1.5 s it takes to
call the host lost: a host that says goodbye (`NetServer::shutdown`) hands over in a quarter of a second.

On the host, next to the journal pump:

```text
handoff.pump_server(&mut server, &journal, |bytes| MyMessage::Handoff(bytes));
// and for each message a client sent:
if handoff::is_handoff_payload(&bytes) { handoff.on_payload(peer, &bytes)?; }
```

On every client, every frame:

```text
handoff.set_appointed(shadow.is_successor());
if let Some(bytes) = handoff.poll_out() { client.send(Channel::GAME_RPC, &bytes)?; }   // a successor's announcement
// for each message from the host:
if handoff::is_handoff_payload(&bytes) { handoff.on_payload(&bytes)?; }
let link = HostLink { last_heard_ms: client.last_heard_ms(), ended };
match handoff.poll(now_ms, link, shadow.promotable()) {
    Some(HandoffAction::Arm(plan)) => {
        let mut server = NetServer::new(server_socket, plan.server_config(my_server_config));
        plan.apply(&mut server)?;                                // every player held, ids kept
        server.start_at_tick(shadow.applied_tick().unwrap());
        armed = Some((server, plan));
        // every tick while armed: server.receive(now); server.advance_tick(|_, _| {}); server.flush(now);
        // and for each ServerEvent::PeerResumed { peer, .. }: handoff.peer_resumed(now_ms, peer);
    }
    Some(HandoffAction::Commit) => {
        let promotion = shadow.promote()?;                       // the world, at promotion.tick
        // simulate on from server.driver().tick(); move what PeerId::SERVER owned to plan.host_seat;
        // host with HandoffHost::with_epoch(plan.epoch); drop the connection to the old host
    }
    Some(HandoffAction::Disarm) => { /* the host is back: server.shutdown(), flush, drop it */ }
    Some(HandoffAction::Promote(plan)) => { /* only without an announced port: see below */ }
    Some(HandoffAction::Resume(target)) => {
        // a second socket; keep the connection to the host until this one is accepted
        attempt = NetClient::connect(second_socket, target.addr, target.client_config(my_client_config), &join)?;
    }
    Some(HandoffAction::StayWithHost) => { /* drop the attempt */ }
    Some(HandoffAction::ReturnToHost) => { /* drop the attempt; no successor answered */ }
    None => {}
}
// when the attempt reports Connected { peer }: handoff.resumed(now_ms, peer); the attempt is the connection now
```

These blocks are sketches of the calls, not compiled examples; the loopback test
`clients_resume_to_the_promoted_successor_with_their_peer_ids` in `crates/regia_net/src/handoff_tests.rs` and
`crates/regia_net_soak/tests/migration_handoff.rs` are complete programs.

What to know:

- `handoff.poll_event()` gives the readout for the game's interface: `HostLost`, `Resuming`, `Armed`, `Promoted`,
  `Resumed`, `HostRestored`, `NoSuccessor`, and on the armed or promoted server `PeerResumed`, `Quorum`,
  `AllResumed`, `NoQuorum`.
- Give every client a server port with `HandoffClient::with_port`: a second socket at the address the host sees the
  client at. An armed server counts ticks from the shadow's tick without simulating, so the clients that arrive sync
  to a running clock; the world goes on at the server's tick when it commits.
- A resume attempt resends its handshake packets every 100 ms and gives up after `takeover_ms`.
- The promoted server needs a resume grace; `plan.server_config` sets 60 s when the game's configuration has none. A
  player that does not come back within it leaves with `Timeout`, as after any dormancy.
- A successor is reached at the address the host saw it at and the port it announced. A game whose players sit
  behind address translation needs a port mapping or a relay for that port, as for hosting.
- Without an announced port (`with_port` not called) a successor has one socket. It then cannot wait for the other
  players: at its turn it gets `Promote(plan)`, gives its connection to the host up and serves on its client socket
  (`NetServer::new(client.into_socket(), ..)`, `plan.apply`, `start_at_tick(promotion.tick)`). A successor in its own
  outage then leaves a host that is still there. It reports `NoQuorum` after 10 s without a majority.
- A successor that committed because its own connection ended, without a majority, also reports `NoQuorum` after
  10 s: the old host may still be serving the others. The game decides whether to go on.
- Dedicated servers do not migrate.

Check a running match with [`net.migration`](../agents/net-migration.md): the `handoff` object of each endpoint.
