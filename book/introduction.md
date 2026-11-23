# Introduction

The Regia Engine Manual. It covers making a game with Regia Engine, from installing the tools to shipping
a packaged build. It starts with the ideas the engine is built on, walks through a first project,
explains each fundamental concept and every engine system in turn, then builds a small 3D game
step by step. The reference chapters at the end list the command-line tools, the content formats,
the render settings and every agent-protocol method.

## What Regia Engine is

Regia Engine is a 3D game engine written entirely in Rust. A game is a Rust crate: you write your
gameplay as ECS systems and components, author levels, prefabs, materials and UI as text files,
and the engine supplies the rest: a window and input on every platform, a physically based
renderer, physics, animation, audio, runtime UI, navigation, a gameplay framework, an editor with
play-in-editor and code hot reload, and the tooling to cook and package the result.

The engine is built as a set of crates around an Entity Component System (`bevy_ecs`, behind
Regia Engine's own `regia_ecs` facade). The pieces you will meet most often:

| Crate | What it gives you |
|---|---|
| `regia` | The umbrella: `regia::run`, `DefaultPlugins`, the prelude |
| `regia_app` | `App`, `Plugin`, schedules, time, the fixed timestep |
| `regia_scene` | `Transform`, hierarchy, levels and prefabs |
| `regia_render` | The renderer, cameras, lights, render profiles |
| `regia_asset` | The asset server, cooked data, `.fpak` archives |
| `regia_framework` | Players, characters, cameras, game rules, scripts, settings, save games, the sim bridge |
| `regia_physics` | Rigid bodies, colliders, queries (rapier underneath) |
| `regia_animation`, `regia_audio`, `regia_ui`, `regia_input` | Animation, audio, runtime UI, input |
| `regia_terrain`, `regia_vfx`, `regia_matgraph` | Terrain and scatter, particles and decals, material graphs |
| `regia_nav`, `regia_ai` | Navmeshes and pathfinding, behavior trees |
| `regia_agent` | The agent protocol server |
| `regia_editor`, `regia_cli` | The editor (`regia-editor`) and the command-line tool (`regia`) |

## Design goals

- **Agent-native.** Every capability a human has in the editor is also available to an AI agent
  through a typed, documented, testable protocol (JSON-RPC, and MCP through `regia mcp`).
  Agents query and edit the scene, load assets, take screenshots, step the simulation, inject
  input and assert on the result. Every agent change is undoable, attributed and audited.
  Everything the engine stores is diffable text, so agent edits review like code. See
  [Agent protocol and MCP](user-guide/agent-protocol.md).
- **Rust first.** Engine, editor, tools and gameplay are all Rust: behavior is Rust code that
  hot reloads into the running editor ([Hot reload](user-guide/hot-reload.md)). The one scripting
  option is opt-in: sandboxed Lua 5.1 mods behind the `lua` feature, for games that want
  player-made content ([Lua mods](agents/lua-mods.md)). Shaders are WGSL.
- **Data you can read.** Levels, prefabs, materials, UI, animation graphs, VFX and audio cues are
  canonical RON. Saving an unchanged file is byte-identical, so `git diff` shows exactly what
  changed. These formats are versioned and frozen: a format only changes together with a
  migrator ([Content formats](reference/formats.md)).
- **One architecture from phones to high-end desktops.** Quality scales through render
  profiles, from Mobile Low to Desktop Ultra plus a Steam Deck profile, not through separate
  renderers ([Render profiles](reference/render-profiles.md)).
- **Physically based by default.** Lights are in lux and candela, the camera is exposed in EV100,
  the display transform is AgX. Get the light levels right and the image follows
  ([The default look](user-guide/default-look.md)).
- **Deterministic when you need it.** A fixed timestep, manual time stepping and an offscreen
  renderer make two runs produce byte-identical screenshots, which is how the engine's own
  golden-image tests and your game's agent tests work.
- **Ships a real game.** Regia Engine's first title is *Direct Order 27*, which drives a large part of
  the feature set and its priorities: terrain, atmosphere and the sim bridge exist because that
  game needs them.

## Platforms

| Platform | Runtime | Editor | Graphics backend |
|---|---|---|---|
| Linux x86_64 (Wayland and X11), including Steam Deck | yes | yes | Vulkan |
| Windows 10/11 x86_64 | yes | yes | DX12 or Vulkan |
| Android arm64-v8a | yes | no | Vulkan |
| macOS | builds and tests on Metal, testing only: no release build ([macOS](user-guide/macos.md)) | not verified | Metal |

The renderer runs on `wgpu`; it never falls back to OpenGL. Every engine change is built for
Linux, Windows and Android. Packaging produces a Linux `tar.gz`, a Windows `zip`, and an Android
APK or AAB, plus a Steam depot layout ([Packaging](user-guide/packaging.md)).

## How this manual is organized

1. **Getting started**: install the tools, create a project, tour the editor, run and package
   your game.
2. **Fundamentals**: the ECS, apps and plugins, scenes, assets and the cook, reflection, input.
   Read these in order; every later chapter assumes them.
3. **Engine systems**: one chapter per system: rendering, materials, terrain, lighting, VFX,
   physics, animation, audio, UI, navigation and AI, the gameplay framework, the sim bridge, hot
   reload, the editor, the agent protocol, packaging, and profiling.
4. **Tutorials**: build a small third-person game from the template, then study the arena sample
   (bots, weapons, match rules) and test it through the agent protocol.
5. **Reference**: CLI, content formats with their versions and migrators, render settings, and
   every agent method.
6. **Agent guide**: task-oriented pages for AI agents driving the protocol. The same pages are
   the exact method parameters and examples.

Every code block marked `rust` in this book is compiled against the engine by
`cargo xtask book-check`, and every RON block that names a format (`ron,material`, `ron,ui`, …)
is parsed by that format's real loader. The same check runs `agent-coverage`: every method the
agent dispatcher registers has a schema entry, a bundled guide topic, a row in the coverage map,
a mention on its guide page, and a test that calls it. Features that are specified but not built yet are marked
**planned** with the specification section that describes them.

## Conventions used here

- `console` blocks show shell commands, with `$` before each command you type.
- Paths such as `docs/specs/13-renderer.md` refer to the engine repository; `book/` pages link to
  each other.
- Component names are nouns (`RigidBody`, `PointLight`), systems are verb phrases
  (`propagate_transforms`), plugins end in `Plugin`.
- Units are SI: meters, seconds, kilograms, radians in code (degrees only where a field says so),
  lux for illuminance, EV100 for exposure.

## Getting help

The engine's specifications live under `docs/specs/`, design decisions in `docs/decisions.md`,
and the rationale for each algorithm, with its public source, in `docs/provenance.md`. When the
manual and the code disagree, the code wins. File the page.
