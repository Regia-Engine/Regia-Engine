# Regia Engine

Regia Engine is an editor-first game engine for 3D and 2D games, written in Rust. You make games as Rust projects
with a dockable editor, play-in-editor, prefabs, a physically based renderer, animation, physics, audio, UI and
networking, and you ship them to Linux, Windows and Android. People and AI assistants get the same capabilities:
the editor and every game can be driven through Regia's agent protocol and its MCP server. Everything runs on your
own machine; Regia needs no account and sends nothing anywhere on its own.

Source: https://github.com/Regia-Engine/Regia-Engine

## Set up with your AI assistant

Open this folder (or paste this repository's URL) in an AI coding assistant that can run commands, and say
*"Set Regia up for me."* It reads `AGENTS.md`, asks you a few questions (which platforms, how you want to work
with it, which template), installs Rust, builds Regia from source, checks your machine with `regia doctor` and
creates your first project. It tells you the size of every download before it starts, never runs administrator
commands for you (it shows you the command), and never accepts anything on your behalf. Prefer doing it by hand?
Follow [Installing Regia Engine](book/start/install.md).

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
| macOS Apple silicon (Metal) | builds from source, for testing | builds from source, for testing |

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
