# Glossary

<!-- index: Glossary -->

One short definition per term, then the chapter that explains it. The
[index](index.md) lists every indexed heading. The [catalog](catalog.md) says whether the
feature is in this version. Nothing here ranks one approach above another.

## Project

<!-- index: Project -->

A project is the Cargo workspace `regia new` writes: the game crate, its launcher, and the
asset and config files the cook reads. The engine checkout stays where it is; the project
path-depends on it. See [Project structure](../user-guide/project-structure.md) and
[Your first project](../getting-started.md).

## Level

<!-- index: Level -->

A level is a saved scene: entities, components and their values, stored as text.
[Scenes, levels and prefabs](../user-guide/scenes.md#levels).

## Prefab

<!-- index: Prefab -->

A prefab is a reusable piece of a scene, also stored as text, that a level can instance.
[Scenes, levels and prefabs](../user-guide/scenes.md#prefabs).

## Cook

<!-- index: Cook -->

The cook turns authored source (glTF, textures, RON documents) into the runtime bytes the game
loads. `regia run` cooks as part of starting a development game.
[What the cook does](../fundamentals/assets.md#what-the-cook-does).

## Pak

<!-- index: Pak -->
<!-- index: fpak | see: Pak -->

A pak is a packed archive of cooked assets (`.fpak`) that a packaged build loads instead of the
loose cook output. [Packaging](../fundamentals/assets.md#packaging).

## Content chunk

<!-- index: Content chunk -->

A content chunk is a cook grouping (`[cook.chunks]`) that decides which assets download or
install together. The user-guide page that exists today is about shared glTF images and which
chunk those images land in. Priorities, pins and delivery are on the agent page.
[Shared glTF images and content chunks](../user-guide/content-chunks.md),
[Content chunks (agents)](../agents/content-chunks.md).

## Quality tier

<!-- index: Quality tier -->

A quality tier is one step of `QualityTier`. `QualityTier::EVERY` lists eight, cheapest first:
MobileLow, MobileHigh, Deck, VeryLow, Low, Medium, High, Ultra. `QualityTier::ALL` is the seven
through v0.5.0 and does not include VeryLow; VeryLow uses Low's baked content
(`content_tier`). Render profiles are a separate list. Settings and the step-down rules are in
[Automatic quality detection](../user-guide/auto-quality.md).

### MobileLow

<!-- index: MobileLow -->

The mobile low tier. On Android, the `Low` graphics preset selects it.

### MobileHigh

<!-- index: MobileHigh -->

The mobile high tier. On Android, every graphics preset other than `Low` selects it.

### Deck

<!-- index: Deck -->

The Steam Deck tier: a 1280×800 preset with a 40 fps cap and temporal upscale, for the Deck as
a Linux target. The platform row is in [Known issues and limits](known-issues.md).

### VeryLow

<!-- index: VeryLow -->

Desktop Low drawn at one third of the output size in each axis and upscaled with SGSR 1
(ADR-7882). It is the step below `Low` for a GPU that misses 30 fps there. Budgets and baked
content follow `Low`.

### Low

<!-- index: Low -->

The desktop low tier.

### Medium

<!-- index: Medium -->

Desktop High settings at a 0.67 render scale.

### High

<!-- index: High -->

The desktop high tier.

### Ultra

<!-- index: Ultra -->

The desktop ultra tier. The offline Cinematic render profile uses this tier for content.

## Render profile

<!-- index: Render profile -->
<!-- index: Cinematic -->

A render profile is a named block of render settings (`RenderProfile::from_preset`). The game
tiers and the offline Cinematic preset are profiles; Cinematic is not one of the eight quality
tiers, and its content tier is Ultra. The generated table is
[Render profiles](../reference/render-profiles.md).

## Listen server

<!-- index: Listen server -->

`NetMode::ListenServer` hosts the session and also has local players. `NetMode` is `Offline`
(the default), `ListenServer`, `DedicatedServer` or `Client`.
[Modes, transports and finding a game](../user-guide/networking.md#modes-transports-and-finding-a-game).

## Dedicated server

<!-- index: Dedicated server -->

`NetMode::DedicatedServer` hosts with no local players and no window, renderer or audio. It is
a separate `server` feature build of the game. [Dedicated servers](../user-guide/dedicated-server.md).

## Lockstep

<!-- index: Lockstep -->

Lockstep runs a match by exchanging inputs, not world state. One host builds a frame of every
seat's input each tick, and every participant steps its own simulation from those frames.
[Lockstep matches](../user-guide/networking.md#lockstep-matches).

## Rollback

<!-- index: Rollback -->

Rollback replays from a stored snapshot when the authoritative state for an older tick arrives.
For a step the game owns, that log is `regia_net::RollbackLog`.
[Predicting a step the game owns](../user-guide/networking.md#predicting-a-step-the-game-owns-spec-120--95).
Lockstep has its own host and seat path on the [lockstep](../user-guide/networking.md#lockstep-matches)
page.

## Prediction

<!-- index: Prediction -->

Prediction is the owning client simulating its own entities ahead of the server from local
input. The server stays the authority and corrects the client when they disagree.
[Predicting your own character](../user-guide/networking.md#predicting-your-own-character-spec-73).

## Interpolation

<!-- index: Interpolation -->

Interpolation blends a remote entity between states the client has already received, so a
snapshot is not shown raw. Relevance and priority decide what is sent.
[Relevance, priority and interpolation](../user-guide/networking.md#relevance-priority-and-interpolation-specs-72-and-74).

## Lag compensation

<!-- index: Lag compensation -->

Lag compensation judges a hit against the world as the shooter's client saw it, within a rewind
window tied to that client's measured delay. It is server-side (`LagCompPlugin`).
[Hitting what you saw](../user-guide/networking.md#hitting-what-you-saw-lag-compensation).

## Relay

<!-- index: Relay -->

A relay is a small rendezvous server. A host registers and receives a join code; a client looks
the code up and, when a direct path fails, can be forwarded. It is not a general game server.
[Finding each other](../user-guide/networking.md#finding-each-other).

## Join code

<!-- index: Join code -->
<!-- index: Six-letter code | see: Join code -->

A join code is the six-letter code a relay gives a registered host. Clients join with that code
and the relay's address. [Finding each other](../user-guide/networking.md#finding-each-other).

## Lobby hub

<!-- index: Lobby hub -->

A lobby hub is a process you run (`regia-lobby-hub`) so players can gather, see presence and
invite each other without a platform account (`regia_net::lobby::hub`, ADR-8559). Steam lobbies
are a different backend and are partial; see [Known issues and limits](known-issues.md).
[Self-hosting a lobby hub](../user-guide/networking.md#self-hosting-a-lobby-hub).

## Agent protocol

<!-- index: Agent protocol -->

The agent protocol is the JSON-RPC API a tool uses to query and edit a running game or the
editor. Methods, error codes and limits are published by `agent.schema`.
[Agent protocol and MCP](../user-guide/agent-protocol.md),
[Agent methods](../reference/agent-methods.md).

## MCP

`regia mcp` exposes the agent protocol as MCP tools, one tool per method, over standard
input/output or over loopback HTTP. [Quickstart for an agent](../start/quickstart-agent.md),
[MCP over HTTP](../agents/mcp-http.md).

## Play in editor

<!-- index: Play in editor -->

Play in editor copies the edit world into a play world and runs it. Stop drops the play world;
the edit world is unchanged. Alt+P starts play when no session is running and no text field has
focus. The agent method is `editor.play`.
[Play-in-editor](../user-guide/editor-guide/play-in-editor.md).

## Character

<!-- index: Character -->

A capsule moved by `CharacterInput` through the physics world.
[Move a character](../how-to/play/character.md),
[Gameplay](../user-guide/gameplay.md).

## Character controller

<!-- index: Character controller -->

The kinematic sweep (slopes, stairs, platforms) behind that capsule.
[Physics](../user-guide/physics.md).

## Movement mode

<!-- index: Movement mode -->

Walking, falling, flying, swimming, or custom.
[Move a character](../how-to/play/character.md).

## Coyote time

<!-- index: Coyote time -->

A jump still accepted shortly after walking off a ledge.
[Move a character](../how-to/play/character.md).

## Health

<!-- index: Health -->

Current and max hit points on an entity.
[Combat and abilities](../how-to/play/combat-abilities.md).

## Damage

<!-- index: Damage -->

A `DamageEvent` applied to `Health` on the next fixed step.
[Combat and abilities](../how-to/play/combat-abilities.md).

## Gameplay tag

<!-- index: Gameplay tag -->

A hierarchical name such as `Combat.Damage.Fire`.
[Gameplay](../user-guide/gameplay.md).

## Ability

<!-- index: Ability -->

A granted action with cost, cooldown, and cancel rules.
[Combat and abilities](../how-to/play/combat-abilities.md).

## Attribute

<!-- index: Attribute -->

A named number with a durable base and a derived current.
[Gameplay](../user-guide/gameplay.md).

## Gameplay effect

<!-- index: Gameplay effect -->

A timed or instant package of modifiers and tags.
[Combat and abilities](../how-to/play/combat-abilities.md).

## Montage

<!-- index: Montage -->

A one-shot animation gameplay starts over the anim graph.
[Montages](../user-guide/montages.md).

## Motion warping

<!-- index: Motion warping -->

Bending a montage's root motion toward a point the game chooses.
[Montages](../user-guide/montages.md).

## Anim graph

<!-- index: Anim graph -->

The state machine, blend spaces, and layers that choose a clip.
[Animation](../user-guide/animation.md).

## Motion matching

<!-- index: Motion matching -->

Picks a locomotion frame that continues the current pose along a path.
[Motion matching](../user-guide/motion-matching.md).

## Navmesh

<!-- index: Navmesh -->

A walkable mesh built from level triangles.
[Navigation](../user-guide/navigation-ai.md).

## Behavior tree

<!-- index: Behavior tree -->

A decision tree whose leaves run tasks.
[Navigation](../user-guide/navigation-ai.md).

## GOAP

<!-- index: GOAP -->

A minimum-cost sequence of actions from a world state to a goal.
[GOAP](../user-guide/goap.md).

## Destruction level 2

<!-- index: Destruction level 2 -->

Pre-fractured chunks and bonds that break from hits. Level 3 is not in this version.
[Physics](../user-guide/physics.md).

## Vehicle

<!-- index: Vehicle -->

A ray-cast wheeled chassis. Trailers, aircraft, hover craft, bikes, and tracked vehicles are
separate bodies on the same page. [Vehicles](../user-guide/vehicles.md).

## Runtime UI

<!-- index: Runtime UI -->

In-game widgets laid out by `regia_ui`.
[UI](../user-guide/ui.md),
[UI and audio for a playing game](../how-to/play/ui-and-audio.md).

## Audio bus

<!-- index: Audio bus -->

A named volume stage (`Master`, `Music`, `Sfx`, `Voice`).
[Audio](../user-guide/audio.md).

## Sound cue

<!-- index: Sound cue -->

An event that picks and plays a wave on a bus.
[Audio](../user-guide/audio.md).

## Dialogue

<!-- index: Dialogue -->

One spoken line chosen by who is speaking and listening.
[Dialogue](../user-guide/dialogue.md).

## Localization

<!-- index: Localization -->

Fluent strings, one directory per locale.
[Localization](../user-guide/localization.md).

## Input action

<!-- index: Input action -->

A named control (`Jump`, `Move`) bound to devices.
[Input](../user-guide/input.md).

## Heightfield

<!-- index: Heightfield -->

The grid of heights a terrain stands on.
[Terrain](../user-guide/terrain-vfx.md),
[Author a terrain](../how-to/world/terrain.md).

## Streaming cell

<!-- index: Streaming cell -->

One square of a cooked streamed level. An HLOD is the merged proxy that stands in for a distant
cell. [World streaming](../user-guide/world-streaming.md),
[Stream a level](../how-to/world/streaming.md).

## Floating origin

<!-- index: Floating origin -->

A whole-cell shift of the local origin so stored positions stay precise.
[World streaming](../user-guide/world-streaming.md).

## Procedural graph

<!-- index: Procedural graph -->

A seeded node graph (`.pgraph.ron`) that places content. A biome is a `.biome.ron` that says
what grows in a kind of place.
[Generate procedural content](../how-to/world/procedural.md).

## Voxel terrain

<!-- index: Voxel terrain -->

Partial. Dual contouring of a supplied field is built. The integer voxel core is not in this
version. [Volume meshes](../user-guide/volume-mesh.md).

## Cinematic

<!-- index: Cinematic -->

An offline render profile above Desktop Ultra. It is not a quality tier.
[Rendering](../user-guide/rendering.md).

## DLSS

<!-- index: DLSS -->

An optional NVIDIA upscaler. The SDK is not in this repository. Vulkan on Linux and Windows.
Android and macOS never link it. The built-in upscaler runs when it is absent.
[Rendering](../user-guide/rendering.md).

## XeSS

<!-- index: XeSS -->

An optional upscaler. The SDK is not in this repository. Windows Vulkan. Other targets are not
linked. [Rendering](../user-guide/rendering.md).

## Lightmap

<!-- index: Lightmap -->

Baked irradiance per texel (`.flmap`) for static meshes.
[Lighting](../systems/lighting.md).

## Material graph

<!-- index: Material graph -->

A node graph compiled into the surface function. Lighting stays the engine's.
[Material graphs](../systems/material-graph.md).

## Raw UDP

<!-- index: Raw UDP -->

The datagram transport a shipped Regia session uses in this version.
[Host a game](../how-to/online/host-a-game.md).

## QUIC

<!-- index: QUIC -->

A partial datagram socket in `regia_net`. Not a supported session transport in this version.
[Networking](../user-guide/networking.md).

## Content handshake

<!-- index: Content handshake -->

The check that a joining client matches the server's protocol version, protocol hash, and
content manifest. [Networking](../user-guide/networking.md).

## Voice chat

<!-- index: Voice chat -->

Capture, codec, relay, and playback of voice on a session. Opus needs the `voice-opus` feature.
[Voice chat](../agents/voice-chat.md).

## Asset id

<!-- index: Asset id -->

The stable id in a source's `.meta` sidecar. Levels and materials refer to the asset by this id.
[Asset ids](../user-guide/asset-ids.md).

## Data table

<!-- index: Data table -->

Rows of one reflected struct, keyed by name, in `.table.ron` or `.table.csv`.
[Data tables](../user-guide/data-tables.md).

## Import rule

<!-- index: Import rule -->

A path glob in `config/import_rules.ron` that supplies importer settings for every matching
source. [Import rules](../user-guide/import-rules.md).

<!-- index: Push to talk | see: Voice chat -->
<!-- index: Reconciliation | see: Prediction -->
<!-- index: Headless server | see: Dedicated server -->
<!-- index: Health endpoint | see: Dedicated server -->
<!-- index: Hole punching | see: NAT -->
<!-- index: Desync bundle | see: Desync -->
<!-- index: Destruction level 3 | see: Known issues -->
<!-- index: Continuous collision detection | see: Swept CCD -->
<!-- index: Inertialization | see: Anim graph -->
<!-- index: Flow field | see: Navigation agent -->
