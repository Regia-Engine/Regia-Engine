# Steam real run: two accounts, two machines

One owner session that checks `regia_steam` against a real Steam client (spec 24 § 4). Everything the
SDK side can prove without Steam is already tested against the mock backend (`cargo test -p regia_steam`);
this page is only the part that needs real accounts. Credentials stay on the machines: sign in to the Steam
client yourself, and never put a password, token or Steam Guard code into a file, a log or a command line.

## What you need

- Two machines (A = host, B = guest), each with a Steam client signed in as a different account, and the two
  accounts friends of each other: both legs create friends-only lobbies, which B joins by id (on the shared test
  app 480, a public lobby would let a stranger take B's slot).
- App 480 (Valve's test app, "Spacewar") or your own app id: set `STEAM_APP_ID=<id>` on both machines.
  App 480 needs no purchase; your own id needs both accounts to own it or be on the app's dev team.
- The Steamworks SDK unpacked outside the repository (`crates/regia_steam/README.md`, `STEAM_SDK_LOCATION`).
- A checkout of the same commit on both machines.

## 1. One-account smoke (each machine)

```
STEAM_SDK_LOCATION=~/sdk/steamworks_1.64 cargo test -p regia_steam --features steamworks \
    --test real_client -- --nocapture steam_client_smoke
```

Expect `test steam_client_smoke ... ok` and no line starting with `skipped:` (a `skipped` line means Steam was
not reachable). It covers init, a lobby create and read-back, ownership, the leaderboard lookup and the
Workshop listing.

## 2. Lobby, Steam sockets match, leave and reconnect, cloud (A and B)

The script is `regia_steam::real_run` (ADR-4896); `real_run_harness.rs` runs the same script on the mock
network, so a failure here is about Steam, not the script. The test is `#[ignore]`d: in an ordinary
`cargo test` it shows as `ignored`. That means not run, never pass. Run it only with `--ignored` and the
setup below; with `--ignored` but no setup it fails with `NOT RUN (<code>)`.

Both machines need the same commit and the same app id. On each, in the checkout:

```
export STEAM_SDK_LOCATION=~/sdk/steamworks_1.64   # where you unpacked the SDK
export STEAM_APP_ID=480                           # or your own app id, the same on both
export REGIA_STEAM_RUN_COMMIT=$(git rev-parse HEAD)
```

On A (host):

```
REGIA_STEAM_TWO_ACCOUNT=host REGIA_STEAM_RUN_LOG=$PWD/host.log \
    cargo test -p regia_steam --features steamworks --test real_client -- \
    --ignored --nocapture --exact two_account_lobby_and_steam_sockets
```

It prints `REGIA_STEAM_LOBBY_ID=<number>`, then waits about 4 minutes for the guest. On B (guest):

```
REGIA_STEAM_TWO_ACCOUNT=guest REGIA_STEAM_LOBBY_ID=<number> REGIA_STEAM_RUN_LOG=$PWD/guest.log \
    cargo test -p regia_steam --features steamworks --test real_client -- \
    --ignored --nocapture --exact two_account_lobby_and_steam_sockets
```

What happens, in order (every step is a `REGIA_RUN` line on the console and a line in the log):

1. A creates a friends-only lobby with its commit and app id in the lobby data (`lobby_created`).
2. B joins by id and checks the lobby advertises the same commit and app id (`lobby_entered`,
   `content_checked status=match`); another build stops B with `content_mismatch`.
3. A sees B join and launches the lobby to its own Steam id (`member_joined join=1`, `server_set`); B
   follows the launch (`server_seen`).
4. B sends `ping.1.<lobby>` over Steam sockets, A answers `pong.1.<lobby>` (`datagram_in`/`datagram_out`).
5. B leaves and joins the same lobby again (`left`, `rejoin`, `lobby_entered round=2`); A logs
   `member_left` and `member_joined join=2`. B reads the launch from the lobby data and runs the echo
   again as round 2: this is the reconnect.
6. B sends `done`, A logs `guest_done` and leaves.
7. Each machine writes `saves/regia_real_run_<role>.frts` through Steam Remote Storage on its own account,
   reads it back, compares, deletes it and checks it is gone (`cloud_round_trip`).
8. Each ends with `result status=complete`, and the test is `ok`. Anything else ends with
   `result status=failed code=<code>` (for example `timeout waiting=<step>`) and the test fails.

The logs hold the app id, the commit, the lobby id and account tags (`acct-<16 hex>`, a fixed hash of
each Steam id so the two logs can be matched without publishing the ids). They never hold a password,
token, Steam Guard code or account name; nothing in the script reads one.

Pass for this section takes both: both tests `ok`, and the verdict in § 4a is `PASS`.

To also see the template's follow branch (`LobbySteamTransport` with `regia::steam::SteamLobbyTransport`
in `follow_lobby`), run your game's client on both machines with the Steam plugin and create/join from its UI or
with the agent method `lobby.create` / `lobby.join`; the link reaches `connected` on B.

## 3. Overlay on Vulkan and DX12

The app is the windowed client of the `net_game` sample built with `--features steamworks`: it runs
`SteamPlugin` (app id from `STEAM_APP_ID`, `steam_appid.txt` or `[steam] app_id` in a `Regia.toml` of the
working directory) and its HUD shows a line in the top-left corner (ADR-7317):

```
STEAM OVERLAY: closed (opened 0, closed 0)
Steam (steamworks): running, app 480
overlay closed (Shift+Tab opens it)
last: no overlay message yet
gpu: <adapter name> (Vulkan)
```

The first row is the one to read (ADR-7349): `STEAM OVERLAY: OPEN (opened 1, closed 0)` while the overlay is up (the
whole block turns yellow), `STEAM OVERLAY: closed (...)` otherwise, with how many `OverlayOpened` and `OverlayClosed`
messages arrived. While the overlay is up the third line reads `overlay OPEN (Shift+Tab closes it)`; the fourth names the last message the
game received, `OverlayOpened` or `OverlayClosed`, with how many of each arrived; the last line is the GPU and backend
the window renders with (`Vulkan` or `Dx12`). `Steam (steamworks): not running` means the client found no Steam
(start Steam and sign in). An agent reads the same state with `steam.status` (`overlay_open`) and the line with
`ui.query {"name": "steam_overlay"}`.

Before you start, on each machine: the Steam client is running and signed in. Then, from the repository root:

```
bash docs/evidence/steam/2026-11-10/steam-owner-session.sh overlay
```

It builds the client (`cargo build -p net_game --features steamworks --bin net_game_client`, in its own cargo
invocation, never together with the server), and puts into `target/steam-session/overlay/` the client, the Steam API
library, `steam_appid.txt` (`480`, or your `STEAM_APP_ID`) and a launcher, `net_game_client.sh`. It ends by listing
the adapters (`adapter <n>: <name> (<backend>, ...)`, the numbering `REGIA_GPU_INDEX` takes). `steam_appid.txt` is
what lets a program Steam did not start pass the plugin's shipping restart check; without it the client logs
`Steam relaunched the app (id 480); exiting this process` and closes.

No server is needed: the client shows its window and HUD while it waits for one. To see avatars as well, start
`cargo run -p net_game --no-default-features --features server --bin net_game_server` in another terminal first.

- **Linux (Vulkan).** Steam draws its overlay in programs it starts, so launch the client from Steam: Steam > Games >
  Add a Non-Steam Game to My Library > Browse, set the file filter to all files, pick
  `target/steam-session/overlay/net_game_client.sh`, add it, and play it from the library (leave Compatibility off).
  Press Shift+Tab once and let go: expect the overlay to draw over the window and stay, the first HUD row to read
  `STEAM OVERLAY: OPEN (opened 1, closed 0)`, and after a second Shift+Tab `STEAM OVERLAY: closed (opened 1, closed 1)`. On a Wayland desktop, run it once more with the launch option
  `WAYLAND_DISPLAY= %command%` (the window then goes through XWayland) and record both. A run from a terminal
  (`... overlay run`) reaches Steam (the line says `running`), but Steam may not draw its overlay there; that is not
  the finding.
- **Windows (DX12).** In Git Bash, run `... overlay` as above; from the adapter list pick a line that says `Dx12`.
  Then start the client with that adapter, either by index or by a part of its name:

  ```
  cd target/steam-session/overlay
  REGIA_GPU_INDEX=<n> ./net_game_client.exe
  REGIA_GPU_NAME=<substring> ./net_game_client.exe
  ```

  The HUD's last line must read `gpu: <name> (Dx12)`; if it says `Vulkan`, the index was not a DX12 entry. Press
  Shift+Tab, same expectation as on Linux. On Windows Steam usually draws its overlay in a game started from a terminal too; if it
  does not, add `net_game_client.exe` as a non-Steam game, set `REGIA_GPU_INDEX` as a user environment variable
  (`setx REGIA_GPU_INDEX <n>`, then restart Steam) and play it from the library.

Record, per backend, whether the overlay draws, flickers or is missing, and what the HUD line said; a missing overlay
on one backend is the finding.

**The overlay trace (ADR-7349).** The launcher sets `REGIA_STEAM_OVERLAY_TRACE`, so every run started from it writes
`overlay-trace-<date>-<time>.txt` (and appends the client's log to `overlay-client-log.txt`) in the overlay folder. Any
game with `SteamPlugin` writes the same trace when you set `REGIA_STEAM_OVERLAY_TRACE=<file>` yourself. The header
says whether Steam injected its overlay (`overlay_renderer_preloaded yes`, `vk_overlay_layer 1`) and which display the
window could use; then one line per event, `t=+<seconds>s frame=<n> ...`:

- `steam callback GameOverlayActivated active=true (edge)` / `(repeat)`: what Steam told the game,
- `game OverlayOpened` / `game OverlayClosed`: what the game acted on,
- `window focus in` / `out`, `key ShiftLeft (Shift) down`, `key Tab (Tab) up`, ...,
- `game asks cursor_grab=... cursor_visible=...`, `cursor hold for the overlay: on/off`, `os set_cursor_grab call #n ...`,
- `renderer surfaces hidden (throttled, not presenting): true/false` (the `net_game` client),
- `heartbeat frames=<n> ...` once a second.

How to read it: no `GameOverlayActivated` line at all means Steam never told the game the overlay opened (what you saw
was not Steam's overlay, or Steam drew it without telling the game); `active=true` on the Shift+Tab press and
`active=false` on the key release means Steam closed it on the release; an `os set_cursor_grab` line between them
means the game took the pointer back. While the overlay is open the engine holds the cursor free and visible
(`SteamSettings::release_cursor_on_overlay`, on by default), so that last line should not appear.

The `net_game` HUD shows its scoreboard while Tab is held; in the Steam build Shift (and an open overlay) keeps it
hidden, so a panel that appears only while Shift+Tab is held is not the scoreboard.

## 4. Cloud round trip across machines

Section 2 already round-trips a file through Steam Remote Storage on each account. The cross-machine
sync needs one account on both machines, which the two-account run cannot do: sign A's account in on B
yourself (the harness never changes accounts), or use one machine and a second install. On A, write a save
with the Steam cloud backend (`SteamRemoteStorage`, or an Auto-Cloud path from `autocloud_document`
configured in the Steamworks partner site), wait for the client to show the sync as done, quit. On B,
start the game and read the save. Pass: the bytes read on B equal what A wrote. Then delete the save on B
and confirm it disappears on A after sync.

## 4a. Judge the two logs

Copy `guest.log` from B next to `host.log` on A (any copy method; the logs hold no secrets) and run:

```
cargo run -p regia_steam --example steam_real_run_verdict -- host.log guest.log
```

It prints one line and exits 0 only for `PASS`:

| Output | Meaning |
|---|---|
| `PASS` | Both logs complete and consistent with each other |
| `NOT RUN (setup_absent)`, `NOT RUN (missing_host_log)`, `NOT RUN (missing_guest_log)` | A log is missing: nothing was proven (exit 2) |
| `INCOMPLETE (incomplete_host)` / `(incomplete_guest)`: missing ... | An endpoint stopped early; the missing steps are listed (exit 1) |
| `FAIL (<code>)`: ... | The logs contradict a pass (exit 1) |

`FAIL` codes: `log_unreadable`, `wrong_role` (logs swapped), `app_id_mismatch`, `commit_mismatch`,
`same_account` (both logs from one account), `lobby_mismatch`, `peer_mismatch` (a record names an account
other than the other endpoint), `wrong_payload`, `content_mismatch`, `cloud_mismatch`, `endpoint_failed`.
Setup codes printed by the test itself as `NOT RUN (<code>)`: `setup_absent`, `bad_role`,
`missing_lobby_id`, `bad_lobby_id`, `missing_commit`, `bad_app_id`, `steam_not_ready`, `socket_failed`,
`request_refused`. Endpoint failure codes in a `result status=failed` line: `timeout`,
`lobby_create_failed`, `lobby_join_failed`, `content_mismatch`, `launch_refused`, `host_lost_lobby`,
`wrong_payload`, `socket_failed`, `cloud_write_failed`, `cloud_read_failed`, `cloud_mismatch`,
`cloud_delete_failed`.

### Template match over Steam (N08)

The second leg plays the `net-game` template over the same two accounts (ADR-5375). The driver is
`net_game::steam_leg`; `samples/net_game/tests/steam_leg.rs` runs the same driver on the mock Steam network,
so a failure here is about Steam, not the script. It is `#[ignore]`d like section 2's test. Keep the same
`STEAM_APP_ID` and `REGIA_STEAM_RUN_COMMIT` exports. On A (host):

```
REGIA_STEAM_NET_LEG=host REGIA_STEAM_NET_LOG=$PWD/net-host.log \
    cargo test -p net_game --features steamworks --test steam_leg_real -- --ignored --nocapture
```

It prints `REGIA_STEAM_LOBBY_ID=<number>` (a new lobby, not section 2's) and waits about 4 minutes. On B (guest):

```
REGIA_STEAM_NET_LEG=guest REGIA_STEAM_LOBBY_ID=<number> REGIA_STEAM_NET_LOG=$PWD/net-guest.log \
    cargo test -p net_game --features steamworks --test steam_leg_real -- --ignored --nocapture
```

What happens (each step is a `REGIA_NET` line on the console and a line in the log):

1. A serves the game on a Steam socket and its agent creates a friends-only lobby with `lobby.create`, the commit
   and app id in the lobby data (`agent method=lobby.create lobby=<id>`). B joins by id, so a stranger on app 480
   cannot take the slot; the two accounts must be friends.
2. B's agent joins it with `lobby.join` (`agent method=lobby.join`, `lobby_entered`) and checks the advertised
   commit and app id (`content_checked status=match`). Another build stops B with `content_mismatch`; a wrong or
   expired id, or a full lobby, stops it at once with `lobby_join_failed`.
3. A sees B (`member_joined`) and launches the lobby to its own Steam id (`launch`). B's client follows the
   launch over Steam sockets.
4. B's agent reads `net.status` with the client link `connected` (`net_status endpoint=client state=connected
   server=<tag>`; the tag is the account of the Steam peer the link reached). A's agent reads the server with one
   peer (`net_status endpoint=server peers=1`).
5. B's scripted bot plays 600 fixed steps (10 s) and stands for 120 more. B logs its `netstats` line
   (corrections, resyncs, verify failures, round trip, loss) and leaves; A logs its server `netstats` and leaves.
   A link that drops during the match ends B with `net_disconnected`.
6. Each ends with `result status=complete`, and the test is `ok`.

Copy `net-guest.log` next to `net-host.log` and run the same verdict command; it tells the two legs apart by
the logs' first line:

```
cargo run -p regia_steam --example steam_real_run_verdict -- net-host.log net-guest.log
```

`PASS` takes both logs complete, the guest `connected` to the host's account and still connected at its report,
the server's `datagrams_in` at least 300 (the match was played, not only joined), and every client within the
template's bounds: at most 20 corrections, at most 2 resyncs and no verify failures. The `NOT RUN` and
`INCOMPLETE` lines are as above. `FAIL` codes for this leg: `log_unreadable` (also one log of each leg),
`wrong_role`, `app_id_mismatch`, `commit_mismatch`, `same_account`, `content_mismatch`, `endpoint_failed`,
`lobby_mismatch`, `peer_mismatch`, `net_not_connected`, `net_disconnected`, `net_corrections`, `net_resyncs`,
`net_verify_failed`, `net_too_short`.
Setup codes printed by the test as `NOT RUN (<code>)`: `setup_absent` (`REGIA_STEAM_NET_LEG` is not set),
`bad_role`, `missing_lobby_id`, `bad_lobby_id`, `missing_commit`, `bad_app_id`, `steam_not_ready`,
`socket_failed`, `link_failed`, `request_refused`. Endpoint failure codes: `timeout`, `content_mismatch`,
`lobby_join_failed`, `launch_refused`, `net_disconnected`.

### Owner checklist (two machines, under 30 minutes)

`docs/evidence/steam/2026-11-10/steam-owner-session.sh` runs every command above; each step prints what to do
next. Build first, outside the 30 minutes (a cold build takes a while):

1. Both machines: Steam client running and signed in (A and B on different accounts, friends). Same commit:
   `git rev-parse HEAD` matches. `bash docs/evidence/steam/2026-11-10/steam-owner-session.sh build`.
2. Both (2 min): `... smoke`. Expect `steam_client_smoke ... ok` and no `skipped:` line.
3. A then B (about 5 min): `... echo host` on A; within 4 minutes `... echo guest <id>` on B with the printed id.
4. A then B (about 3 min): `... net host` on A; `... net guest <id>` on B with the new id.
5. Copy `guest.log` and `net-guest.log` from B into A's session folder. On A: `... verdict`. Expect two `PASS`
   lines.
6. Both (5 minutes, manual): `... overlay`, then the overlay check of section 3. Record what you see and what the
   HUD's Steam line said.
7. Fill the table in `docs/specs/24-steam.md` § 4 and keep the session folder (four logs, the verdict lines).

Never type a password, token or Steam Guard code into the script; it asks for none.

## 5. Record the result

Fill the table in `docs/specs/24-steam.md` § 4 ("Real-run results") with the date, the app id, the two
machine descriptions, the commit, and pass, fail or not run per row; keep `host.log`, `guest.log` and the
verdict line with it. A row stays `not run` unless its step is in both logs and the verdict is `PASS`. Report a failure with the full test output only
after removing any account name you do not want published.
