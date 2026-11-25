# {{name}}

A networked game from the Regia `net-game` template: a dedicated server and a client of one shared
simulation, over encrypted UDP.

| Crate | What |
|---|---|
| `game` (`{{crate_name}}`) | The simulation both sides run: replicated avatars, predicted movement, interpolated proxies, lag-compensated shots, lobby following |
| `server` (`{{crate_name}}_server`) | The headless dedicated server: no window, no renderer, no audio |
| `client` (`{{crate_name}}_client`) | The windowed game: boxes on a ground plane, WASD to run, Space to fire, Tab for the scores, with a minimap, a radar, player markers and a target bracket (`game/src/hud.rs`) |

## Play

The server runs under the engine's dedicated-server host: `server.toml`, the console, `/health`,
bans, the level rotation and graceful shutdown come with it (spec 76), and the game's link binds
the game port. From the project folder:

```sh
regia server --clients 2
# builds the server and the client crates, prints `join code ABC123 relay 127.0.0.1:<port>`,
# and starts two headless bot clients that join by that code
```

By hand:

```sh
cargo run -p {{crate_name}}_server -- --level levels/arena.level.ron --relay local
# prints: listening 0.0.0.0:7777 key <server key>
#         join code ABC123 relay 127.0.0.1:7800
cargo run -p {{crate_name}}_client -- --join ABC123
cargo run -p {{crate_name}}_client -- --join ABC123 --bot
cargo run -p {{crate_name}}_client -- --connect 127.0.0.1:7777 --key <server key>
```

The server's settings live in `server.toml` in its directory (`REGIA_SERVER_DIR`, else the
executable's directory; `regia server` uses `build/server-dev/`). Type `help` at its console
(`--console`) for `status`, `players`, `kick`, `joincode`, `quit`. `--relay local` runs a relay
inside the server on `127.0.0.1:7800`; `--relay HOST:PORT` registers with a relay elsewhere.

`regia` needs its `client` or its `server` feature, and with both `regia::run` hosts a server, so
the server and the client are never built in one cargo invocation: plain `cargo build` / `cargo run`
build the game and the client; the server is always `-p {{crate_name}}_server`. Avoid `--workspace`.

A join code pins the server's public key; so does `--key`. Without either a client accepts any key,
which stops a listener but not an attacker in the middle. The server keeps its key in
`server.key` (`net.identity_file`).

## Package

```sh
regia package --server --platform linux    # the headless server: binary, server.toml, content
regia package --platform linux             # the client
```

## Test

```sh
cargo test -p {{crate_name}}
```

`game/tests/e2e.rs` runs a server and two bot clients in one process for a scripted minute over
real encrypted UDP with a lossy conditioner, and checks that every process agrees on positions and
scores afterwards; a second test replays the same match over the in-memory network twice and
compares every number; a third walks the lobby flow (create, join, ready, launch, connect).
`game/tests/hosted.rs` runs the server under the engine's host and two clients that join by join
code, play and leave, at fixed ticks.

## Where to go next

- Replace `move_avatars` with your character movement; keep it in the predicted schedule so the
  owner predicts it and the server runs the same code.
- Add components with `app.replicate::<C>(..)` on both sides in the same order.
- Steam: lobbies come from `regia::steam`, and a Steam id server is reached with
  `regia::steam::SteamSocket` passed to `NetLink::connect`. `follow_lobby` does that for a Steam id launch
  when you insert `LobbySteamTransport(Arc::new(regia::steam::SteamLobbyTransport::new(..)))`; the branch is
  tested with a stand-in transport and, in `regia_steam`, against the mock backend.
- The book page "Build a networked game" walks through every piece.

Strip points are marked `TODO(template)`.

## Release builds

`regia new` writes `.cargo/config.toml` so a plain `cargo build` does not embed this
machine's directories in the game binary (ADR-10370). It appends this machine's home
directory as the last `--remap-path-prefix`. If you set `RUSTFLAGS`, restate those
flags there, with the home directory last. Read the project directory from the
`CARGO_MANIFEST_DIR` environment variable; the compile-time `env!("CARGO_MANIFEST_DIR")`
embeds the absolute path.
