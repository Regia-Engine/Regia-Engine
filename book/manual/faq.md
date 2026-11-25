# FAQ

<!-- index: FAQ -->

Short answers. The limit on a partial feature is on [Known issues and limits](known-issues.md),
not repeated here in full.

## Where do I start?

A person follows [Quickstart for a person](../start/quickstart-human.md). An agent follows
[Quickstart for an agent](../start/quickstart-agent.md). The longer tour is
[Your first project](../getting-started.md). The three doors are on the
[introduction](../introduction.md#where-to-start).

## Is this feature in this version?

Look it up in the [feature catalog](catalog.md). `built` means the inventory records it as
built. `partial` means it works with the limit on the known-issues page. `deferred` means it is
parked and is not part of this version. Module rows and per-flag rows stay in
`docs/manual/feature-inventory.csv`; the catalog does not copy them.

## What does an agent error code mean?

Codes are the strings in `error.data.code`. The universal codes every method can answer with,
and the rest of the shared table, are on [Errors](../agents/protocol.md#errors). A method's own
codes are on its row in [Agent methods](../reference/agent-methods.md) and in `agent.schema`.
`errors.explain` returns the long form and the fix. This manual does not keep a second list:
a code that is not in those sources is not a code.

Agent error codes are lowercase ASCII, digits and `_`, except the `F0001`-style codes in that
errors table. Do not invent a spelling.

## How do I print the manual?

Open `docs/manual/manual.html` and use the browser's print command.
[Printing the manual](print.md).

## Why does macOS not match Linux and Windows?

macOS builds and tests on Metal. It is testing only (ADR-009, ADR-264), not a package target
for the dedicated server, and the editor is not verified there. The row is on
[Known issues and limits](known-issues.md). [macOS](../user-guide/macos.md) is the platform page.

## Where is the sample game?

It is not in this version. ADR-8588 dropped it. Project templates and starter content remain.
Starter content was still thin at the 2026-11-01 truth pass (templates and samples, not a
packaged starter set). [Known issues and limits](known-issues.md).

## How do I upgrade a project's content files?

`regia upgrade-project` lists the plan. `regia upgrade-project --write` rewrites older files
and keeps a `.bak`. [Upgrade notes](upgrade.md).

## The catalog says partial. Can I call the feature anyway?

Read the one-line limit first. A partial row is not a promise that the missing piece is present.
OpenXR does not load a headset runtime in this tree. DLSS and XeSS are not bundled. A Steam
sockets match has not been proven by a two-account session in the notes this manual trusts.
The [catalog](catalog.md) status and the [known-issues](known-issues.md) sentence are the
contract.

## The character falls through the floor

`PhysicsPlugin` is missing, or `use_rapier_character_controller` was not called, so the
flat-plane query is still installed. [Move a character](../how-to/play/character.md).

## The locked cursor does not hide the pointer

On Windows and X11 a Locked grab is sent as Confined. On macOS, Confined is sent as Locked.
[Input](../user-guide/input.md).

## The HUD theme key in Regia.toml does nothing

There is no `[ui] theme` key. Use `ActiveTheme` or `ui.theme_select`.
[UI](../user-guide/ui.md).

## A wall does not fracture when shot

Destruction level 2 needs a cooked `.destructible.ron`. Level 3 (runtime carving) is not in
this version. [Physics](../user-guide/physics.md).

## An ability call returns available: false

`AbilityPlugin` is not on that app. `regia_ai::abilities` is a different type.
[Combat and abilities](../how-to/play/combat-abilities.md).

## A spoken line has no caption

The `.ftl` key or the `.subs.ron` cue is missing, or captions are off in `GameSettings`.
[Dialogue](../user-guide/dialogue.md).

## Cook skipped a file

`unsupported source extension` means that file is not an importer in this build, or a pipeline
feature such as `usd_import` was turned off. It is skipped and is not in the pak. The game can
still open it by path through `assets://` if game code reads the file.
`asset_id_conflict` means a `.meta` id does not match `.assetids.ron`. Nothing is rewritten and
nothing is cooked. Fix the sidecar or the manifest row, then run the command again.
[Cook a project](../how-to/create/cook-a-project.md).

## export usda exits usda_empty

The glTF binary has no triangle mesh. The destination is left as it was.
[Import a model](../how-to/create/import-a-model.md).

## A load fails with unavailable

`unavailable:<chunk>` means the asset's chunk is not mounted. Request or install that chunk,
then load again. The process does not treat this as a missing file with no chunk.
[Content chunks](../user-guide/content-chunks.md).

## Join codes, QUIC, and Steam lobbies

`session.connect` with a join code returns `join_code_unsupported` (`F0002`). The net-game
client `--join` resolves the code. An agent connects by address after that. QUIC is in the
tree and is partial. A shipped session uses raw UDP. Steam lobby calls return
`lobby_unavailable` when the feature is off, the client is not running, or the platform has no
lobby backend. [Host a game](../how-to/online/host-a-game.md).

## Terrain cook and streaming

A cook of a terrain folder uses `*.terrain.layers.ron`, not the generic texture cook.
`regia cook --stats` prints the tiled size. `regia terrain source-inspect` does not resample.
A grid a level cannot place fails later with `level_terrain_mapping`. `runtime_map.loadable`
on that report is the check. Opening a level from source spawns it whole. Streaming starts
after `regia cook`, when `regia run --level` finds a world index.
[Author a terrain](../how-to/world/terrain.md),
[Stream a level](../how-to/world/streaming.md).

## Procedural verify and Sky Lab

`regia procgen verify` compares bytes and does not write the bake directory. The command-line
bake has no physics and no terrain plugin. Sky Lab changes clouds. It does not wet the ground.
Weather is `assets/config/weather.ron`.
[Rain, snow and wet surfaces](../agents/weather-precipitation.md).
[Generate procedural content](../how-to/world/procedural.md).

## Which renderer opens?

The renderer opens Vulkan and DX12 on Windows (Vulkan is the default pick), Metal on macOS
testing builds, and Vulkan on Linux and Android. The product viewer uses WebGPU or WebGL2.
VeryLow is a quality tier. It does not appear as its own column on the generated
render-profile table. Cinematic is an offline profile.
[Quality and upscalers](../how-to/picture/quality-and-upscalers.md).
