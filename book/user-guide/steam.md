# Steam

<!-- index: Steam sockets -->
<!-- index: Steam Input -->
<!-- index: Steam cloud -->
<!-- index: Lobby -->

`regia_steam` integrates Steamworks, and the CLI stages and uploads Steam builds. Steam is opt-in:
without the `steam` feature the engine ships with no Steam code. The owner creates the Steam app.
Credentials for upload come from the environment. This page does not record a completed real match.
The 2026-11-05 versions note (ADR-2828) recorded Steam sockets and lobbies as mock-tested. The owner
procedure is [Steam real run](steam-real-run.md). Hosting over raw UDP, which does not need Steam, is
[Host a game](../how-to/online/host-a-game.md).

## Runtime integration

Enable the `steam` feature and use `regia::steam`:

```rust
use regia::prelude::*;

fn configure(app: &mut App) {
    app.add_plugins(regia::steam::SteamPlugin::default());
}
```

Without the `steamworks` feature (or on Android), the plugin compiles to a graceful no-op:
`SteamState::Unavailable` and every call degrades to `SteamError::Unavailable`, so the game keeps
running. The real `steamworks` backend links Valve's Steamworks SDK at build time
(`STEAM_SDK_LOCATION`), and only on desktop. The SDK and its redistributable are never committed to
this repository (`docs/specs/24-steam.md`, `docs/03` § 5).

## One shared Steam client

The engine owns the one Steam connection. Steam allows a single API instance per process, so a
host (a game shell) must **not** call `steamworks::Client::init` itself; it asks the engine:

<!-- ignore-reason: needs the `steamworks` feature of regia_steam, which the book check does not build -->
```rust,ignore
fn host_system(steam: Res<Steam>) {
    // `None` when Steam is not running (or on the mock backend): degrade, do not init.
    if let Some(client) = steam.client() {
        let _friends = client.friends(); // raw steamworks-rs calls on the shared handle
    }
}
```

- `Steam::client()` returns the shared `steamworks::Client` (a cheap clone). It exists only with
  the `steamworks` feature. The host must depend on the **same `steamworks` version** as
  `regia_steam` (0.13.x, `docs/03` § 5), or the type does not unify.
- The app id comes from project settings (explicit, `SteamSettings`, `Regia.toml` `[steam]`,
  `STEAM_APP_ID`, `steam_appid.txt`); the plugin initializes once, before the first frame.
- The plugin runs `run_callbacks` once per frame in `PreUpdate`. Hosts must not pump callbacks.
- `SteamStatus` (resource): `running`, `offline`, `user_id`, `app_id`, `overlay_open`. Read it for UI
  and gating; it is refreshed every frame.
- Overlay: `OverlayOpened` / `OverlayClosed` messages and `OverlayActive`. Gameplay input contexts
  of local players 0 to 7 are suspended while it is open (player 8 and up keep `Gameplay`; set
  `suspend_contexts` to `None` and handle `OverlayOpened` for more players); set `SteamSettings::pause_on_overlay` to pause game time too.
- Rich presence: `steam.set_presence("status", Some("In a match"))`, `set_presence(key, None)` to
  unset, `steam.clear_presence()` to clear everything (call on disconnect).
- Shutdown: the plugin flushes stats, clears presence and shuts the API down on app termination.
  `Steam::shutdown(&mut self)` does the same earlier; it is idempotent. Clones of the client that
  a host still holds keep the SDK alive until dropped.
- Tests and CI: `SteamPlugin::with_backend(MockSteam::shared())` runs everything with no Steam
  running (`MockSteam::set_offline`, `inject(SteamEvent::OverlayActivated { .. })`, `init_calls`,
  `presence_calls`). The agent method is [`steam.status`](../agents/steam.md).

## Achievements, stats and invites

- `config/achievements.ron` lists the project's achievements: `(id, display, stat?, threshold?, hidden?)`; the
  `id`s and stat names must match the app on the Steamworks partner site (`SteamSettings::achievements_path`
  picks another file).
- `steam.unlock("FIRST_BLOOD")` unlocks one; `steam.stat_add("kills", 1)` adds to an integer stat and returns the
  new value, and an achievement whose `threshold` the new value crosses unlocks automatically. Both raise
  `AchievementUnlocked { id }`. They reach Steam's servers with `steam.store_stats()` (its result arrives as
  `StatsStored`) or the flush on app exit.
- A friend who joins through the overlay or the friends list arrives as `JoinRequested { friend, connect }`, where
  `connect` is the string the game set as the `connect` rich presence key. It comes from the network: check it
  before dialing or parsing it (at most `MAX_CONNECT_BYTES`, 1024).

## Steam Input

Steam Input is opt-in. Insert `SteamInputSettings { enabled: true, player: 0 }` before the plugin
runs (or flip it at runtime). Off, nothing below runs and controllers come through `gilrs` as
before.

- **Contexts are action sets.** Each context in `config/input.ron` is a Steam Input action set
  (`Gameplay` is `GAMEPLAY`, `Main Menu` is `MAIN_MENU`). The enabled context with the highest
  priority for the player picks the set; it is activated when it changes or a controller connects.
- **Actions.** A Regia Engine action with a gamepad binding in a context becomes the Steam action
  `<context>_<action>` (lower case, `_` for anything else): `Button` is digital, `Axis1D` an analog
  trigger, `Axis2D` an analog stick. `Axis3D` is rejected. The values are held on
  `InputInjection` while non-zero and released at zero, so keyboard and mouse bindings of the same
  action keep working.
- **UI navigation.** Every set also has eight actions, `<context>_ui_up`, `_ui_down`, `_ui_left`,
  `_ui_right`, `_ui_accept`, `_ui_cancel`, `_ui_prev`, `_ui_next`. The bridge reports their edges as
  `GamepadButtonChanged` messages (D-pad, A, B, shoulders) on device `STEAM_INPUT_PAD_DEVICE`, which
  is what `ui_gamepad_navigation` reads, so menus stay navigable when Steam hands the game actions
  instead of a gamepad. Ship a controller configuration that maps the D-pad, left stick, A, B and
  shoulders to them.
- **Manifest.** `regia_steam::action_manifest(&InputConfig)` returns the in-game actions file text
  (a set per context, English titles); the agent method `steam.input_manifest` returns the same
  text for the running app. Upload it with the app's Steam Input configuration on the Steamworks
  partner site.
- **Manifest from the command line.** `regia steam input-manifest` writes the same text from
  `config/input.ron` to `dist/steam/game_actions_<appid>.vdf` (the app id comes from `[steam]` in
  `Regia.toml`). `--out <file>` picks another path and needs no app id; `--stdout` prints it.
  `regia package --steam` writes the file into its `steam/` folder as well. If Steam Input cannot
  express the config, packaging logs a warning and continues.
  During development Steam reads the file from its `controller_config` folder (Valve's Steam Input
  documentation). For release, upload it on the partner site.
- **Sleep and resume.** A suspend or resume (on the Deck, also the pair the engine raises after a
  sleep) releases every action the bridge holds and every UI button it reported down. The next
  frame initializes Steam Input again and re-activates the action set, so a button held when the
  Deck went to sleep is not stuck after it wakes (ADR-526).
- **Prompts.** `regia_steam::action_prompts(&config, context, action, family)` turns an action's
  gamepad bindings into glyph labels and texture keys ([Steam Deck](steam-deck.md)). Steam's own
  per-origin glyph images are not wired yet.
- **Not done yet:** action set layers, several players on several controllers (player 0 reads the
  first controller), Steam's per-origin glyph images, and the Deck's floating text input. A template
  that also emulates a gamepad makes menus receive each press twice; ship an action-only template.

The agent methods are [`steam.input`](../agents/steam.md) and `steam.input_manifest`.

## Steam cloud saves

Steam cloud saves back the `SaveGame` system (see [Gameplay](gameplay.md)): wrap the
Steam backend in `SaveStore::new(SteamRemoteStorage::new(&steam))` (see [Save flow](../agents/save-flow.md)):
slots are written through Steam Remote Storage as `saves/{slot}.frts`, not through Auto-Cloud.
`--steam` packaging writes `steamcloud.md`, the Auto-Cloud configuration doc for files under
`user://`; a game that only publishes that doc and never uses `SteamRemoteStorage` does not sync
`SaveGame` slots.

## Leaderboards, ownership and timeline markers

Leaderboards go through the framework's `Leaderboard` resource, so the game never touches the SDK and the same code
runs offline (`LeaderboardPlugin::local()`) and in tests (`MockSteam`):

```rust,ignore
use regia_framework::leaderboard::{Leaderboard, LeaderboardDisplay, LeaderboardRange, LeaderboardSpec};

app.add_plugins((SteamPlugin::new(AppId(480)), SteamLeaderboardPlugin));

fn finish_lap(mut boards: ResMut<Leaderboard>, lap: Res<LapTimer>) {
    boards.declare(LeaderboardSpec::new("fastest_lap").ascending().display(LeaderboardDisplay::TimeMilliseconds)).ok();
    boards.submit("fastest_lap", lap.millis, &[]).ok(); // keep-best by default
    boards.request("fastest_lap", LeaderboardRange::AroundUser { before: 4, after: 5 }).ok();
}
```

Answers arrive on later frames as `LeaderboardMessage`s and in `boards.entries("fastest_lap")`. Ownership and the
game-recording timeline are on `Steam`:

```rust,ignore
let soundtrack = steam.dlc_installed(AppId(1_234_560)).unwrap_or(false);
steam.timeline_mode(TimelineMode::Playing).ok();
steam.timeline_event(&TimelineEvent::new("Boss down").icon("steam_achievement").clip(TimelineClip::Featured)).ok();
```

Limits, error codes and the agent methods are on
[Steam leaderboards, ownership and timeline (agents)](../agents/steam-services.md). With the real client, attaching
UGC to an entry is not available yet (ADR-2340).

## Multiplayer over Steam: lobbies and Steam sockets

The code below is in the tree and is tested against `MockSteam`. ADR-2828 (2026-11-05) recorded that
limit. A two-account session on a real Steam client is
[Steam real run](steam-real-run.md). Fill `docs/specs/24-steam.md` § 4 from that run. A row stays
`not run` until its step is in both logs and the verdict is `PASS`.

`regia_steam::SteamLobbyPlugin` (add it after `SteamPlugin`) adds `regia_net`'s `LobbyPlugin` when it is not there
yet and, once Steam is ready, gives the `Lobbies` resource the Steam lobby backend (ISteamMatchmaking) and a
`SteamLobbyTransport` (ISteamNetworkingMessages). `lobby.launch` then hosts the session on a `SteamSocket` and
members connect to the owner's Steam id over Steam's relay network. It is the same `regia_net` session as over raw
UDP: every datagram goes to Steam unreliable, and `regia_net` does reliability, ordering and fragmentation. A Steam id
appears in session addresses as a synthetic IPv6 address `fd52:6567:6961:0:…` whose port is the channel
(`steam_peer_addr` / `steam_id_of`); the channel is `lobby.steam_channel` in `net.ron` (default 0). By default any
peer may open a session (the session handshake authenticates); `SteamLobbyPlugin { members_only: true }` accepts
only members of the current lobby. Without Steam (feature off, client not running, Android) nothing is attached and
every lobby call reports `lobby_unavailable`. Tests run several users without the SDK on one `MockSteamNet`
(`MockSteam::join_net`); `MockSteamNet::set_default_link(MockLink::profile("bad-wifi"))` (or `set_link` per user) runs
them on the soak harness's impaired links in virtual time (`advance_ms`), see
[Testing without a network](networking.md#testing-without-a-network). With `SteamWorkshopPlugin`, the lobby key `mods.content_set` keeps players with different
mods apart ([Steam Workshop](../agents/workshop.md)).

The lobby methods and events are on [net lobbies](../agents/net-lobbies.md); the two-account check is
[Steam real run](steam-real-run.md).

## Staging and upload

The owner creates the Steam app and does the partner-site steps. This CLI stages depots and
invokes steamcmd. It does not log in to partner tools, and it has no `--retries` field.

```console
$ regia package --platform linux --steam
$ regia package --platform windows --steam
$ regia steam input-manifest
$ regia steam layout --family deck --check
$ regia steam upload --branch beta --dry-run
```

`regia steam upload` regenerates the VDFs from `[steam]` in `Regia.toml`, points steamcmd at
the staged depots, and runs `+run_app_build`. Credentials come from the environment only
(`STEAM_USERNAME`, `STEAM_PASSWORD`, `STEAM_TOTP`) — never from `Regia.toml`, VDFs or logs. `steamcmd` takes them
on its command line, which other local users can read while it runs: upload from a dedicated build account, on a
machine or mount with `hidepid=2` when others can log in to it.
`--branch default` publishes to the branch players get and needs `--live` plus a confirmation
(`--yes` in CI); `--dry-run` prints the steamcmd invocation without uploading.
`input-manifest` writes the in-game actions file from `config/input.ron`.
`layout --check` exits 1 when an action has no gamepad binding. Families are `xbox`,
`playstation`, `switch`, and `deck`. What Steam Input does not cover yet is in the Steam Input
section above (action-set layers, several controllers, per-origin glyphs, Deck floating text).

## Shipping updates: what a patch costs players

Steam delivers updates as deltas over fixed-size chunks of about 1 MiB (SteamPipe's public
documentation), so the size of an update is decided by how many chunk-sized windows of your paks
changed bytes. Regia's pak writer keeps entries sorted by asset id at 4096-byte alignment, so
editing one asset moves only the chunks from that asset's bytes through the end of the pak — every
earlier chunk is byte-identical and Steam reuses it. `regia assets pak-diff` measures the effect
before you upload:

```console
$ regia assets pak-diff dist/linux/old/MyGame/content dist/linux/new/MyGame/content
patch estimate (1 MiB chunks)
  assets: 1 changed, 41 unchanged
  chunks: 118 unchanged, 3 changed, 0 added, 0 removed
  estimated patch download: 3 MiB of 121 MiB new bytes
note: estimate: fixed-size chunks (~1 MiB) per Steam's public SteamPipe documentation; a real Steam patch also compresses and reuses depot chunks
```

Each side is a `.fpak` file or a packaged build directory (paks pair up by relative path; a chunk
pak only in the new build counts as fully added). Per asset — by its asset id from the pak TOC —
the report lists added, removed, changed (the stored bytes differ), moved (same bytes at a new
offset, when a sibling's size change shifted the layout) and unchanged. `--json` prints the same
report for CI (fail the build when an estimate regresses), and `--chunk-size BYTES` changes the
model's chunk size. The download number is an estimate: a real SteamPipe build also compresses
chunks, shares them across depots and reuses chunks a player already has, so reality is smaller.
To keep updates cheap in practice: put big rarely-changed content in their own chunks (a change in
`base` never re-sends `arena`), avoid renames that re-sort before an asset's id, and prefer
same-size edits to hot assets (a resized texture moves every later chunk of its pak). The agent
method `assets.pak_diff { old, new, chunk_size? }` runs the same report from a running app with
the `tools` permission; see [Content chunks](../agents/content-chunks.md) § Shipping updates.

### Absolute build paths (v0.5.1)

`regia package --steam` and `regia steam upload --dist` resolve relative output/distribution
folders against the command's working directory before writing the VDFs. `contentroot` and
`buildoutput` are absolute paths, so steamcmd's script directory cannot change which folder is
uploaded. Windows drive paths use ordinary `C:\...` spelling without `canonicalize`'s `\\?\`
prefix (ADR-7871). Regenerate VDFs after moving a distribution directory.
