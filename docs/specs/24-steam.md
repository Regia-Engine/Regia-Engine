# Spec 24 — Steam integration (`regia_steam`)

Required for Direct Order 27's launch; built in M19, **after the engine is declared stable (M18)**.
Optional for other games (`steam` feature). Desktop only.

## 1. Dependencies and licensing
- `steamworks` 0.13 (MIT/Apache-2.0 Rust bindings) over Valve's Steamworks SDK. The SDK and its
  redistributable (`libsteam_api.so`, `steam_api64.dll`, `libsteam_api.dylib`) are covered by Valve's
  Steamworks SDK Access Agreement, not an open-source licence. **ADR required in the Steam milestone**
  recording: what `steamworks-sys` vendors, that the redistributable ships only inside Steam builds,
  and a `deny.toml` exception scoped to these crates. *(ADR-020: no `deny.toml` exception is needed. `steamworks` and
  `steamworks-sys` are MIT OR Apache-2.0, the SDK and `steam_api` binaries are never committed, and `regia package
  --steam` copies the redistributable from `STEAM_SDK_LOCATION` into Steam packages only; `cargo deny` runs with every
  feature on, `regia_steam/steamworks` included. ADR-5629.)*
- No Steam code in runtime crates outside `regia_steam`; games without the feature contain none of it.

## 2. Runtime features
| Area | Behaviour |
|---|---|
| Init | `SteamPlugin { app_id }`. Shipping: `restart_app_if_necessary`. Dev: `steam_appid.txt` written by the CLI. Callbacks pumped in `PreUpdate`. Steam absent → `SteamState::Unavailable`, game continues (dev, non-Steam builds). |
| Overlay | Verified with wgpu on Vulkan (Linux) and DX12 (Windows; in the owner's Windows VM, `06` § 8); Metal with the macOS port. `OverlayActive` resource; when active, gameplay input contexts are suspended and the game may pause. *(ADR-7349, as-built: `SteamSettings::release_cursor_on_overlay`, default on, makes the plugin hold `regia_platform::CursorRelease` from `OverlayOpened` to `OverlayClosed` — and on a shutdown with the overlay open — so every OS window shows a free, visible cursor and a game that re-grabs the pointer every frame or on focus churn cannot take it back from the overlay; the game's own `Window` values wait and are applied again after the close.)* |
| Achievements, stats, leaderboards | `config/achievements.ron` (ids, display keys, stat thresholds) + API `steam.unlock("id")`, `steam.stat_add("kills", 1)`. Dev tooling: reset all for the test account. |
| Cloud saves | `SaveGame` backend trait gets a `SteamRemoteStorage` implementation; or Steam Auto-Cloud with documented `user://saves` paths per OS. Conflict handling surfaced to the game. M19 delivers Auto-Cloud of `user://` (settings and game-written files — all DO27 needs); the `SteamRemoteStorage` backend lands with `SaveGame` in M20. |
| Friends, rich presence, invites | Rich presence keys from game state; `JoinRequested { connect }` event when a friend joins via overlay or friends list (DO27 maps it to its own connect flow). |
| Lobbies / matchmaking | ISteamMatchmaking wrapper: create, list with filters, join, lobby data, chat. |
| Networking | Steam Networking Sockets + Steam Datagram Relay exposed as (a) a `Transport` for `regia_net` (Tier 2) and (b) a raw datagram API so games with their own netcode (DO27's renet stack) can use SDR for NAT traversal instead of their own relay — the game's decision. *(ADR-1877: both ride ISteamNetworkingMessages, which uses the same relay network; `SteamSocket` is the `regia_net` datagram socket and the raw API. ISteamNetworkingSockets connections were not used because their own reliability would duplicate the `regia_net` stack. ADR-5629.)* |
| Steam Input | Generates the Steam Input action manifest from `config/input.ron` contexts (action sets) and actions; Regia Engine input reads Steam Input when enabled; button glyphs from Steam for prompts. *(ADR-526: prompts come from the engine's glyph families (`deck_glyphs::action_prompts`, `steam.glyphs`); Steam's per-origin glyph images (`GetGlyphPNGForActionOrigin`) are not implemented, a v0.5.x gap. ADR-5629.)* |
| Workshop | Upload/download mod paks (Regia Engine pak + metadata), subscribed items mounted at `mods://` by the VFS with load order; hash reported for lobby matching (DO27 matches sim pack and shell pack by hash). *(ADR-2418: `regia_steam::workshop` — `WorkshopService` with `MockWorkshop` and `RealWorkshop` (ISteamUGC, `steamworks` feature), `SteamWorkshopPlugin` mounts installed items as `workshop.<id>` at the end of the `ModsVfs` load order, remounts updates in place, unmounts unsubscribed items, and keeps the lobby key `mods.content_set` of an owned lobby equal to the content-set hash; uploads are checked as a `mods://` mount and carry `regia.mods.content=<hash>`; agent `steam.workshop_status`, `_upload` (permission `tools` plus the game's `allow_agent_upload`), `_subscribe`, `_unsubscribe`, `book/agents/workshop.md`.)* |
| Steam Deck | Detection; default render profile `Deck` (1280×800, 30/40/60 fps caps, FSR-style upscale); minimum text size rules in `regia_ui` for Deck readability; `ShowFloatingGamepadTextInput` for text fields; controller-first navigation. Goal: Deck Verified. *(batch v020: `DeckFrameCap` 30/40/45/60 with `DeckPanel` refresh ranges, `DeckFrameCapSetting`, `UiDevicePolicy`, sleep detection, `vfx_lab --deck-check`; ADR-213.)* |
| Misc | DLC ownership, app ownership, Steam language → `Localization`, Steam timeline/game-recording markers from game events (optional). |

## 2a. Leaderboards, ownership and timeline markers (ADR-2340)
- **Leaderboards** are platform neutral: `regia_framework::leaderboard::Leaderboard` (resource) over a
  `LeaderboardService` (none by default, `LocalLeaderboards` in memory, Steam through
  `regia_steam::SteamLeaderboardPlugin`). A board is declared by name (`LeaderboardSpec`: sort ascending or
  descending, display numeric / seconds / milliseconds, upload method keep-best or force, create-if-missing) and
  found once. Uploads carry up to 64 `i32` details; downloads are global rank windows (1-based), a window around
  the local user, or the user's friends with global ranks, at most 1000 rows. A UGC handle can be attached to the
  local user's entry (mock and local service; the `steamworks` 0.13 bindings do not expose it). Results arrive as
  `LeaderboardMessage`s and per-board `BoardState`; nothing blocks a frame.
- **Ownership**: `Steam::owns_app` (`BIsSubscribedApp`), `dlc_installed` (`BIsDlcInstalled`), `app_installed`,
  `app_install_dir`, `app_ownership`.
- **Timeline**: `Steam::timeline_mode` (playing, staging, menus, loading screen), `timeline_state` /
  `timeline_clear_state` (the state tooltip), `timeline_event` (icon, title, description, priority 0..=1000, start
  offset, duration, clip priority none / standard / featured). `Steam::timeline_log` keeps what was sent.
- **Agent**: `steam.leaderboard_find`, `_upload`, `_download`, `_attach_ugc`, `_entries`, `_list`, `steam.owns_app`,
  `steam.timeline_mode`, `_state`, `_event`, `steam.timeline` (`book/agents/steam-services.md`).
- All of it degrades like the rest of this crate: without Steam every call fails with `Unavailable` /
  `leaderboard_unavailable`, and the game keeps running.

## 3. Packaging and upload
- `regia package --platform linux|windows --steam` produces the depot layout plus
  `app_build_<appid>.vdf` / `depot_build_<depotid>.vdf` from `[steam]` in `Regia.toml`
  (`app_id`, `depots`, `branches`).
- `regia steam upload --branch <beta|...>` runs `steamcmd +run_app_build` with credentials from the
  environment only (never written to disk by Regia Engine). Default branch upload never targets `default`
  without `--live` and a confirmation.
- **Linux builds for Steam** target the Steam Linux Runtime 3.0 ("sniper") glibc baseline (2.31) so the
  binary runs on every supported distro and the Deck: `regia package` links against glibc 2.31 with
  cargo-zigbuild and checks the floor (ADR-148; building inside the sniper SDK container with
  `--glibc host` is equivalent); the package is verified inside the runtime (`SteamLinuxRuntime_sniper/run`);
  CI job provided.
- Agent protocol: `package` with `--steam` and `steam_upload` always require owner confirmation (§ 5 of
  spec 23).

## 4. Acceptance (Steam milestone)
- With Valve's public test app (480): overlay opens on Vulkan (Linux) and DX12 (Windows); achievement unlock +
  reset; stats persist; cloud save round trip between two machines; rich presence visible to a friend;
  invite → `JoinRequested` received; lobby create/join between two accounts; Workshop upload + subscribe +
  mount of a test pak; Steam Input action sets switch with input contexts.
- Linux build from the sniper container runs on Arch, Ubuntu LTS and SteamOS (Deck or Deck-resolution VM
  check), suspend/resume on Deck keeps the game alive.
- DO27's Regia Engine build uploaded to a Steam playtest branch with `regia steam upload`, installed and
  played through Steam on Linux, Windows and Steam Deck.

### Real-run results (ADR-3169)

SDK-side code and mock tests are done (`lobby_sockets.rs`, `net_link_steam.rs`, the `net_game` template's
`a_steam_id_launch_is_followed_through_the_steam_transport`). The owner's two-account session follows
`book/user-guide/steam-real-run.md`; fill in one row per check.

| Check | Result | Date | App id | Machines |
|---|---|---|---|---|
| One-account smoke (`steam_client_smoke`) | not run | | | |
| Lobby create (A) and join by id (B) | not run | | | |
| Launch reaches the follower (`steam:<id>`) | not run | | | |
| Datagram echo over Steam sockets | not run | | | |
| Overlay on Vulkan (Linux) | not run | | | |
| Overlay on DX12 (Windows) | not run | | | |
| Cloud save round trip | not run | | | |
| Leave and reconnect, second echo (`real_run`, ADR-4896) | not run | | | |
| Per-account cloud round trip in the two-account run (ADR-4896) | not run | | | |
| Two-log verdict (`steam_real_run_verdict`, ADR-4896) | not run | | | |
| Template match over Steam: agent `lobby.create`/`lobby.join`, `net.status` connected, corrections <= 20 and resyncs <= 2 per client (`steam_leg_real`, ADR-5375) | not run | | | |
| Template-match verdict (`steam_real_run_verdict net-host.log net-guest.log`, ADR-5375) | not run | | | |

A row is `pass` only with both endpoint logs (`host.log`, `guest.log`), the commit both ran, and the
verdict line `PASS` kept with it (ADR-4896). Without the owner's session every row stays `not run`; a
default `cargo test` lists the two-account test as `ignored`, which is not a result.

## 5. Store-neutral host seam (P-21, ADR-8159)

`regia_steam::store::Platform` abstracts identity, presence, overlay, achievement,
cloud-write notification and shutdown over None, Steam and an unavailable GOG
stub. The owner-approved P3 source is `shell-regia/game/src/platform.rs` at
DO27 d6ed37677 (regia-absorb-1); adaptation and attribution are in the port ledger.
`select` borrows the existing `Steam` resource instead of starting another
client. Hosts install `SteamPlugin` only when they request Steam; `StorePlugin`
records that request and registers read-only `store.status`. None is the default;
missing Steam and GOG select None without file IO or store calls. GOG has no
Galaxy SDK. Cloud-write notification is inert, matching the source; the existing
save backends and Auto-Cloud continue to own actual synchronization.
The store agent guide defines status reasons and the host lifecycle. Local-id
entropy/persistence stays in the host. No simulation math or content format changes.
