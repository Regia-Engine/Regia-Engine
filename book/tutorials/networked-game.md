# Build a networked game

This tutorial walks through the `net-game` template: a dedicated server and a client of one shared
simulation, over encrypted UDP, with client-side prediction, interpolation, lag compensation and
lobbies. The same code is the workspace sample `samples/net_game`, so everything below is compiled
and tested in CI (ADR-2039).

## 1. Create the project

```sh,walkthrough=new,template=net_game
regia new "Tag Yard" --template net-game
cd tag_yard
```

(`net-game` and `net_game` name the same template.)

| Crate | Package | What |
|---|---|---|
| `game/` | `tag_yard` | The simulation both sides run; its `client` feature adds the presentation (`view`) |
| `server/` | `tag_yard_server` | The headless dedicated server: no window, no renderer, no audio |
| `client/` | `tag_yard_client` | The windowed game |

The server turns on `regia`'s `server` feature (`default-features = false`), so it links no window,
render or audio crate; the client turns on `client`. `regia` needs one of the two, and with both
`regia::run` hosts a dedicated server, so never build the server and the client in one cargo
invocation: `cargo build` and `cargo run` in the project build the workspace's default members (the
game and the client), and the server is built on its own with `cargo build -p tag_yard_server
--release`. Do not use `--workspace`.

## 2. Play it

The quickest way is `regia server --clients 2` in the project folder: it builds the server and the
client crates (each in its own cargo invocation), runs the server with a relay inside it, and starts
two headless bot clients that join by the printed join code. By hand:

```sh
cargo run -p tag_yard_server --release -- --level levels/arena.level.ron --relay local
# listening 0.0.0.0:7777 key 3f1c...e9
# join code ABC123 relay 127.0.0.1:7800
cargo run -p tag_yard_client -- --join ABC123
cargo run -p tag_yard_client -- --join ABC123 --bot
cargo run -p tag_yard_client -- --connect 127.0.0.1:7777 --key 3f1c...e9
```

WASD runs, Space fires at the nearest other player. Each tag adds to your `Score::tags` and the
target's `Score::tagged`.

The server crate always runs under the engine's dedicated-server host, so its flags are the host's
(`--port N`, `--level PATH`, `--relay local|HOST:PORT`, `--console`, `--config PATH`, `--bind ADDR`,
`--max-players N`, `--shared-port`, `--bots N`, ...), its settings are `server.toml` in its directory
(`REGIA_SERVER_DIR`, else the executable's directory), and it keeps its key in `server.key`. Console,
bans, `/health`, signals and exit codes (0, 2 configuration, 3 port, 4 content, 70 crash) are on
[the dedicated server](../agents/net-dedicated-server.md) page.

| Client flag | Meaning |
|---|---|
| `--connect ADDR` | The server |
| `--join CODE` | Look a join code up on the relay and connect with the server's key pinned |
| `--relay ADDR` | The relay for `--join` (default `127.0.0.1:7800`, the port of a server started with `--relay local`) |
| `--key KEY` | Pin the server's key. Without it (and without `--join`) the client accepts any key: that stops a listener, not an attacker in the middle |
| `--bot`, `--bot-for N` | Play the scripted bot instead of the keyboard (for `N` ticks, then stand still) |
| `--headless --ticks N` | No window; print what the client saw after `N` ticks |
| `--clients N` | With `--headless`: `N` clients in one process, each with its own socket (1 to 64); a load and soak tool |
| `--host` | With `--headless`: also run the dedicated server in this process, on 127.0.0.1 (play-in-editor with N clients, over real encrypted UDP) |
| `--exit-after-frames N` | With a window: close after `N` rendered frames, exit 0 (the smoke flag every template's game takes) |
| `--conditioner SPEC`, `--seed N` | Shape this client's outbound datagrams (a profile such as `wan`, or `delay=40,jitter=10,loss=0.02`; see [net match](../agents/net-match.md)) |

Client exit codes: 0 done, 2 bad arguments, 3 the port cannot be bound.

To check the whole loop in one process, without a window or a second terminal (a script, CI), let
the client host the server and run a bot for 120 ticks:

```sh,walkthrough=smoke,template=net_game
cargo run -p tag_yard_client -- --headless --host --bot --ticks 120
```

It prints `client 0 connected=true sealed=true` and the avatars it saw. The windowed client also
takes `--exit-after-frames N`.

Packaging: `regia package --platform linux` does not handle this layout yet. It looks for a root
package or a `launcher/` crate and stops with "a project needs a root package or a launcher/
package". Until it does, build the client in release; the binary is
`target/release/tag_yard_client` (under `$CARGO_TARGET_DIR` if you set it), and it runs from any
folder:

```sh,walkthrough=package,template=net_game
cargo build --release -p tag_yard_client
```

The dedicated server has its own `regia package --server` (`docs/specs/76-dedicated-server-package.md`).

## 3. The transport: `NetLink`

`regia::net::NetPlugin` runs replication inside an app: it fills a `NetNode`'s inbox and outbox.
`NetLink` moves those frames over a socket. `NetLinkPlugin` inserts an idle link; the game starts
it:

```rust,ignore
let opts = link_options(LinkClock::Wall, ConditionerConfig::default());
app.world_mut().resource_mut::<NetLink>().host_udp::<NetGameWire>(bind, &opts)?;      // server
app.world_mut().resource_mut::<NetLink>().connect_udp::<NetGameWire>(server, &opts)?; // client
```

`NetGameWire` is the game's `NetGame` identity: its `GAME_ID` and `PROTOCOL_VERSION` are checked at
the handshake, so a client of another game or another wire version is refused. Bump the version on
every wire change.

| Setting (`LinkOptions`) | Template value | Why |
|---|---|---|
| `crypto` | `CryptoMode::Required` | Every link sealed, loopback included ([transport encryption](../agents/net-crypto.md)) |
| `server_key` | the `--key` flag | Pins the server, so an attacker in the middle is refused |
| `conditioner` | off (`--conditioner` on the client; `REGIA_NET_CONDITIONER` for a dev-build server) | Seeded loss, delay, jitter, duplicates and reorders for testing |
| `clock` | `LinkClock::Wall` | Tests use `LinkClock::Manual`: one app update is one fixed step, no sleeps |

Any `DatagramSocket` works with `NetLink::host` and `NetLink::connect`: `UdpTransport`, the in-memory
`LoopbackSocket` of tests, or `regia::steam::SteamSocket`, which carries the same datagrams over
Steam's relay network to a Steam id.

## 4. Replicated state

```rust,ignore
app.replicate::<Avatar>(ReplicateOptions::default())
    .replicate::<Score>(ReplicateOptions::default())
```

Registration order feeds the protocol hash, so register in the same order on both sides:
`NetGamePlugin` does it once for both. When a peer joins, the server's `sync_avatars` spawns an avatar
with `Replicated { transform: PositionOnly }`, `NetOwner(peer)`, `NetPredicted` and `HitShapes`;
when the peer leaves, the avatar goes.

## 5. Prediction

The client predicts its own avatar so movement answers at once:

```rust,ignore
app.add_net_input::<Pad>()
    .predict::<Transform>(Tolerance::position(0.001))
    .add_predicted_systems((move_avatars, judge_shots).chain());
```

`Pad` is the per-tick command (`move_x`, `move_z`, `fire`, `aim`); its `on_missing` keeps running but
never repeats a shot. `move_avatars` runs on the server for every player and in the owner's
prediction for its own avatar; `PredictionCx::simulates` picks which. When the server's state at a
tick differs by more than 1 mm, the client rolls back and re-runs its unacknowledged commands. See
[net prediction](../agents/net-prediction.md).

## 6. Interpolation

Other players' avatars are proxies: the client shows them between two server states, about one
send interval behind. The template uses `LinkInterpolationPlugin`, which reads the link's clock, so
the manual-clock tests show the same proxies on every run. See
[net interpolation](../agents/net-interpolation.md).

## 7. Lag compensation

A shot is judged on the server, but against the targets as the shooter saw them: the input stream
carries the shooter's view tick, and `LagCompensation::raycast` rewinds every `HitShapes` to it.
`judge_shots` excludes the shooter's own avatar and counts each command tick once, even when
prediction re-runs it. See [net lag compensation](../agents/net-lag-compensation.md).

## 8. Lobbies

`lobby.rs` follows a lobby (spec 89) to the dedicated server:

1. One player creates a lobby; the others list and join it.
2. Everyone marks ready (`Lobbies::set_ready(true)`).
3. The owner calls `launch_when_ready(&mut lobbies, server_addr)`, which publishes the address.
4. `follow_lobby` connects every member's idle `NetLink` there, the owner's too.

Insert `Lobbies` with a backend first: `LocalLobbyHub` for tests and LAN builds, the Steam backend
in a Steam build. A launch to a Steam id (`LobbyServer::Steam`) is followed over Steam sockets when
the app holds a `LobbySteamTransport` (in a Steam build, wrapping `regia::steam::SteamLobbyTransport`);
without one it is refused once with a logged `no_transport` and the member leaves the lobby. See
[net lobbies](../agents/net-lobbies.md).

## 9. The end-to-end test

`game/tests/e2e.rs` (in the sample: `samples/net_game/tests/e2e.rs`):

| Test | What it proves |
|---|---|
| `a_scripted_minute_over_encrypted_udp_agrees` | A server and two bot clients over real UDP on 127.0.0.1, encryption required, key pinned, 2 % loss with jitter, reorders and duplicates, play 3600 ticks. Afterwards every process agrees on every avatar's position (within 1 mm) and score, both links are sealed, the conditioner dropped traffic, at least 90 % of shots hit through lag compensation, every shot was judged in the past, no failed verification, at most 20 corrections and at most 2 lead resyncs (a clock step, not a state divergence) |
| `the_in_memory_minute_replays_bit_for_bit` | The same match over the in-memory network, twice: every position, score, shot count and prediction counter repeats |
| `a_lobby_launch_brings_both_clients_to_the_server` | Create, join, ready, launch: both clients connect through the lobby and see both avatars |
| `a_steam_id_launch_is_followed_through_the_steam_transport` | A launch to a Steam id, with a stand-in lobby transport installed: both clients connect and see both avatars |
| `a_steam_id_launch_without_a_transport_is_refused_once` | The same launch without a transport: the links stay idle and the members leave the lobby instead of retrying every frame |
| `a_player_who_leaves_loses_the_avatar_everywhere` | A client goes silent: after the server's 10 s silence timeout its avatar is gone on the server and on the other client |

The sample has three more tests that the template leaves out: `tests/render_smoke.rs` renders a client headless (lavapipe in CI) and checks that an agent screenshot shows both players' colors, `tests/agent_smoke.rs` reads and steers the link over the agent protocol, and `tests/process_pair.rs` starts the real `net_game_server` and `net_game_client --headless --clients 2 --bot` as two processes, plays a minute over encrypted UDP in real time with the `home` conditioner on the server, and compares what each process printed.

The e2e tests run without a GPU, on a manual link clock: no sleeps, no timing budgets. The process pair needs no GPU either; it takes a real minute, since each binary paces itself, and asserts no durations.

## 10. Watching it with the agent

Run a client or a server built with the `agent` feature and use the agent protocol:

| Method | What it shows |
|---|---|
| `net.status`, `net.peers` | The link's endpoint report, published every frame under the label `server` or `client` |
| `net.crypto` | Whether the link is sealed and the server key |
| `net.conditioner_set` | Changes the link's conditioner live |
| `net.prediction`, `net.input_buffer` | Prediction counters and the server's input buffer |
| `net.interpolation` | Proxy delay and starvation |
| `lagcomp.stats`, `lagcomp.last_queries` | What the shots were judged against |
| `lobby.status`, `lobby.launch` | The lobby and its launch |

See [net link (agents)](../agents/net-link.md) for the link's own report and error codes.

## 11. Make it yours

- Replace `move_avatars` with your character movement and keep it in the predicted schedule.
- Give `NetGameWire` your own `GAME_ID`.
- Replace the boxes in `game/src/view.rs` (the `client` feature: meshes, camera, light, input) with
  your characters, and add a score HUD.
- The server crate already runs under the supervised production host (health endpoint, console,
  `server.toml`, bans, join codes): the game's `HostedLink` binds the game port, and `--shared-port`
  serves the session layer of spec 77 on the same port. See
  [the dedicated server](../agents/net-dedicated-server.md) and `regia package --server`.
