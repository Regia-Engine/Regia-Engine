# Regia Engine

Regia Engine is an editor-first game engine for 3D and 2D games, written in Rust. You make games as Rust projects
with a dockable editor, play-in-editor, prefabs, a physically based renderer, animation, physics, audio, UI and
networking, and you ship them to Linux, Windows, Android and the Steam Deck. People and AI assistants get the same
capabilities: the editor and every game can be driven through Regia's agent protocol and its MCP server. Everything
runs on your own machine; Regia needs no account and sends nothing anywhere on its own.

**Current release: v0.7.2 (pre-release).** Download it from
[GitHub Releases](https://github.com/Regia-Engine/Regia-Engine/releases/tag/v0.7.2) or
[regiaengine.com](https://regiaengine.com). Website: https://regiaengine.com

## Download

| Platform | Download | Notes |
|---|---|---|
| Linux x86_64 | `regia-v0.7.2-linux-x86_64.tar.gz` | glibc 2.31 or newer; Vulkan |
| Windows x86_64 | `regia-v0.7.2-windows-x64.zip` | DX12 or Vulkan; no runtime to install |
| macOS Apple silicon | `regia-v0.7.2-macos-arm64.dmg` | unsigned, for testing (right-click > Open the first time) |

Each archive holds the `regia` command-line tool and the editor. Check the files against `SHA256SUMS` on the release
page. Prefer building from source? See [Install by hand](#install-by-hand).

## What you get

- **Editor:** dockable workspaces and themes, panels in their own windows, universal search, an outliner with a query
  language, one undo history across levels and asset editors, autosave and crash recovery.
- **Levels and worlds:** blockout brushes, terrain with tunnels and multi-level maps, water (rivers, lakes, an FFT
  ocean, underwater views), foliage and scattering, weather.
- **Rendering:** a physically based renderer with global illumination, baked lightmaps, virtual shadow pages, TAA and
  upscaling, quality tiers from phones and the Steam Deck to desktop GPUs.
- **Gameplay:** frozen, versioned rules formats (battle royale, RTS, PvE, racing), match results, a lockstep
  simulation kit, vehicles, animation graphs and procedural locomotion.
- **Multiplayer:** a sealed transport with session resume, lobby hub and relay, hosting on your own dedicated server
  without exposing your address, capture and replay.
- **Tools:** material and logic graphs with debugging, a sequencer with movie render, a profiler and frame debugger,
  git with LFS locks, `regia package` for shippable builds.
- **AI assistants as first-class users:** every editor and game feature is on the agent protocol, with an MCP server,
  a full manual in `llms-full.txt`, and guided setup.

## Set up with your AI assistant

Open this folder (or paste this repository's URL) in an AI coding assistant that can run commands, and say
*"Set Regia up for me."* It reads `AGENTS.md`, asks you a few questions (which platforms, how you want to work
with it, which template), installs what is needed, checks your machine with `regia doctor` and creates your first
project. It tells you the size of every download before it starts, never runs administrator commands for you (it
shows you the command), and never accepts anything on your behalf.

| Mode | Who drives | What the assistant does |
|---|---|---|
| 1 Local-first (default) | you | installs, explains, runs the commands you ask for |
| 2 Co-pilot | you | also connects to your running editor to look, play-test and edit when you ask |
| 3 Assistant builds | the assistant | builds the game through the editor and asks you only for decisions |

What happens, what the assistant will and will not do, privacy and uninstalling:
[Set up with your AI assistant](book/start/assistant-setup.md).

## Install by hand

1. Install the prerequisites and Rust, then build the command-line tool and the editor:
   [Installing Regia Engine](book/start/install.md).
2. Check the machine: `regia doctor`.
3. Create and run a project: [Your first project](book/getting-started.md).

## Platforms

| Platform | Editor | Games |
|---|---|---|
| Linux x86_64 (Vulkan) | yes | yes |
| Windows x86_64 (DX12 or Vulkan) | yes | yes |
| Android aarch64 (Vulkan) | no | yes, built from Linux or Windows |
| Steam Deck | no | yes, built from Linux |
| macOS Apple silicon (Metal) | yes, for testing | yes, for testing |

**Known issue in v0.7.2:** Android games do not start on Qualcomm Adreno GPUs (for example the Fairphone 5); the fix
lands in v0.8.0. Mali GPUs (for example the Pixel 9 Pro) are fine. Full list:
[release notes](https://github.com/Regia-Engine/Regia-Engine/releases/tag/v0.7.2).

## Status and what is next

Regia is pre-1.0: formats that are frozen come with migrators (`regia upgrade-project`), and every release lists its
known issues. Next is v0.8: a performance pass on Linux and Windows, the Adreno fix, iOS on real devices, and
multiplayer rate-control improvements.

## Documentation

- The manual: [book/introduction.md](book/introduction.md); for assistants, `llms.txt` (an index) and
  `llms-full.txt` (the user manual in one file).
- Driving the editor and games from an assistant: [Agent protocol](book/user-guide/agent-protocol.md).

## License

Regia Engine is free and open source. The engine is licensed under the Mozilla Public License 2.0
([LICENSE](https://github.com/Regia-Engine/Regia-Engine/blob/main/LICENSE)); the project templates and samples are
under MIT-0 or CC0 1.0, so the games you make from them are yours under any terms you choose; the manual is under
CC BY 4.0. Copyright Sphenisca Regia LLC. Nothing needs to be accepted to use or build Regia.

Contributions are welcome under the Contributor License Agreement: see
[CONTRIBUTING.md](https://github.com/Regia-Engine/Regia-Engine/blob/main/CONTRIBUTING.md) and
[CLA.md](https://github.com/Regia-Engine/Regia-Engine/blob/main/CLA.md).
