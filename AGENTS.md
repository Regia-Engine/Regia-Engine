<!-- regia:user-agents-md -->
# Regia Engine: instructions for AI assistants

Regia Engine is an editor-first game engine for 3D and 2D games, written in Rust. Games are Rust projects; they
build for Linux, Windows and Android (macOS builds from source, for testing). The editor and every game can be
driven by an AI assistant through Regia's agent protocol and its MCP server (`regia mcp`). Everything runs on the
user's own machine: Regia needs no account and sends nothing anywhere on its own. It is free and open source: the engine is
under the Mozilla Public License 2.0, the project templates and samples under MIT-0 or CC0, the manual under
CC BY 4.0 (source: https://github.com/Regia-Engine/Regia-Engine).

If the user asked you to set Regia up, follow "Setting Regia up" below. If they asked something else, answer it from
`llms.txt` (an index of the manual), `llms-full.txt` (the user manual in one file) or the `book/` folder.

## Rules

1. Ask the setup questions one at a time, each once, labeled `(Question N of 6 · <id>)` with the ids below. Label
   each consent `(Consent · <step id>)`. If the user says "you decide", use the default and say which one you used.
2. Never accept anything on the user's behalf: no license, terms or prompt of any tool (Rust, Android SDK,
   Xcode, a GPU driver). Regia's own license needs no acceptance to use or build it; show where it is if asked.
3. Never run a command that needs administrator rights (sudo, doas, pkexec, an elevated PowerShell, an installer
   that asks for elevation). Show the exact command and why; the user runs it; then continue.
4. Never download anything before telling the user what, from where, and how large, and getting a yes.
5. Do not send the user's files, paths or hardware details anywhere. Regia has no telemetry; do not add any
   upload, account or tracking the user did not ask for, and do not paste their machine details into web searches.
6. Change files only in the engine folder and the user's projects folder. Anything outside (a shell profile,
   `PATH`, an MCP client file) needs its own consent, with the exact file and line.
7. Do not edit the engine's source unless asked. Games live in their own projects.
8. In the local-first mode, do not connect to the editor or edit the project unasked. In modes 2 and 3, make edits
   through the agent protocol where it has a method, so they land on the editor's undo stack.
9. If a step fails, run `regia doctor --json` and follow the `fix` of the failed check. Do not improvise system
   changes.
10. Never pipe a downloaded script into a shell, and never use simulated clicks or keys on Regia's windows.
11. Until a new shell has Regia on its `PATH`, call `regia` by its absolute path
    (`<engine>/target/release/regia`, on Windows `regia.exe`).

## Setting Regia up

Say what Regia is (the first paragraph above, in your words), name the three working modes in one line (the user
picks one at Question 4), and say that you will ask about six questions.

1. **(Question 1 of 6 · machine)** Name the OS, CPU, RAM, GPU and free disk you see (from the OS now; from
   `regia doctor --json` once `regia` exists), and ask whether to set Regia up on this machine. A build needs
   at least 8 GB of RAM (16 GB recommended) and about 40 GB of free disk; say so if the machine has less.
2. **(Question 2 of 6 · install_from)** A git clone of the repository (default; needs `git`) or a release
   archive `regia-engine-<version>.tar.gz` from the repository's Releases page, when it has one. Both build from
   source; there is no prebuilt download. Then the engine folder (defaults below). Then bootstrap, in this order:
   - Prerequisites first. Each is an administrator command: show it and why, and the user runs it.

     | OS | Prerequisites before the first build |
     |---|---|
     | Linux | A C/C++ compiler: Debian/Ubuntu `build-essential`, Fedora `gcc gcc-c++ make`, Arch `base-devel`; plus that distribution's row of `book/start/install.md` § 2 (headers and the Vulkan driver); `git` for a clone |
     | Windows | Visual Studio Build Tools with the C++ workload, installed before `rustup-init` runs; `git` for a clone (Git for Windows), or use the release archive |
     | macOS | The Xcode command-line tools (`xcode-select --install`, a system dialog; they also provide `git`) |

   - Rust **(Consent · rust_toolchain)**, skipped when `rustup` is already installed: download `rustup-init`
     (Windows: `rustup-init.exe`) for this host from `https://static.rust-lang.org/rustup/dist/<target>/`
     together with the `.sha256` file next to it, where `<target>` is `x86_64-unknown-linux-gnu`,
     `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` or `aarch64-apple-darwin`. Compare the file's SHA-256
     (`sha256sum`, `shasum -a 256` or `Get-FileHash`) with the first field of the `.sha256` file; on a mismatch,
     delete it and stop. Run it as the user: `rustup-init -y --default-toolchain none`. Until a new shell picks it
     up, call `cargo` and `rustup` from `~/.cargo/bin` (Windows `%USERPROFILE%\.cargo\bin`).
   - Engine **(Consent · engine_source)**: say the size first (up to about 1 GB; the Releases page lists the
     archive's size). Clone: `git clone --depth 1 https://github.com/Regia-Engine/Regia-Engine <engine folder>`.
     Archive: download it, `tar -xzf regia-engine-<version>.tar.gz`, and move the folder where it will stay.
   - In the engine folder, `rustup toolchain install` installs the compiler `rust-toolchain.toml` pins (about
     300 MB of downloads, about 1.3 GB installed; part of this consent).
   - Build **(Consent · build_tools)**: say the sizes first (the first build downloads about 150 MB of crates from
     crates.io, about 1.5 GB unpacked, and writes about 6 to 10 GB under `target/`; it takes 20 to 60 minutes),
     then run `cargo build --release -p regia_cli -p regia_editor` in the engine folder. For an archive, then run
     `<engine>/target/release/regia engine verify .`.
   - Run `regia doctor --platform <linux|windows|macos> --json` by its absolute path and fix each failed check with
     its `fix` (the rules above still apply). Adding `<engine>/target/release` to `PATH` in the shell profile is
     **(Consent · shell_path)**; without it, keep using the absolute path.
3. **(Question 3 of 6 · targets)** This machine always; Android, Windows-from-Linux and Steam Deck are optional and
   each adds downloads (`regia doctor --platform android|windows|deck --json` lists what is missing, and
   `book/start/install.md` § 5 has the Android steps). Default: this machine only. Ask
   **(Consent · android_tools)** or **(Consent · cross_tools)** with the sizes before installing them.
4. **(Question 4 of 6 · mode)** Default 1. The user can switch any time.
   1. Local-first: you drive, I explain and run commands you ask for.
   2. Co-pilot: as 1, and I connect to your editor to look, test and edit when you ask.
   3. I build: I build the game through the editor and ask you only for decisions.

   **(Question 4b · client)**, only for 2 and 3 when you cannot tell which assistant app you run in.
5. **(Question 5 of 6 · template)** `regia new --help` lists the templates (`blank`, `first_person`,
   `third_person`, `top_down`, `quick`, `net-game`). Default `first_person`.
6. **(Question 6 of 6 · project)** Name (default `My Game`) and projects folder (default `~/RegiaProjects`).

Create the projects folder if it does not exist, then the project:
`regia new "My Game" --template first_person --path ~/RegiaProjects`. Warm the build
**(Consent · warm_build)**, saying it compiles the game crates first: `cargo build -p my_game_launcher` in the
project (the crate is the snake_case name plus `_launcher`). Run it once with `regia run`, then open the editor with
`regia editor`.

Modes 2 and 3 **(Consent · connect)**: write the editor connection into the game project only, with absolute
paths: for Claude Code, `<project>/.mcp.json` in the shape of `dist/mcp/regia.mcp.json.example`; for any other
client, the command line `<regia> mcp --project <project>` in its MCP settings (or `--http` for a client that cannot
start processes). Then say: "I wrote the editor connection into your project. Start a new session of your assistant
in `<project>` and approve the `regia` server when it asks." In that session, check the link with `agent.hello`.

Default engine folders: Linux and macOS `~/Regia/regia-engine`, Windows `%LOCALAPPDATA%\Regia\e` (short: deep build
paths). Every project refers to the engine folder by path, so it must stay where it is.

## Working modes

| Mode | You (the assistant) | Set up with |
|---|---|---|
| 1 Local-first | teach, explain, run commands on request | nothing extra |
| 2 Co-pilot | connected to the editor over MCP; inspect, screenshot, play-test, edit on request | the project's MCP entry |
| 3 Assistant builds | build, test, screenshot, iterate; ask for decisions | the project's MCP entry; `--headless` runs the editor without a window |

Switch by adding or removing the project's `regia` MCP entry; the project's `.regia/agent.toml` permission profiles
decide what the protocol may change.

## Everyday commands

| Command | Does |
|---|---|
| `regia new "Name" --template first_person` | create a project |
| `regia run` | cook and run the game in a project |
| `regia editor` | open the editor on a project |
| `regia mcp --project DIR` | the MCP server (stdio); `--http` for clients that cannot launch processes, `--headless` starts an editor without a window |
| `regia agent call <method> '<json>'` | call the agent protocol once (`agent.guide` lists the guide pages) |
| `regia doctor --json` | check the toolchains, drivers and platform tools |
| `regia package --platform linux` | build a shippable game (`windows`, `android`) |

## More

- `llms.txt`: the manual's index. `llms-full.txt`: the user manual in one file.
- `book/start/`: install, assistant setup, editor tour, running, first agent check.
- `book/agents/onboarding.md`: the setup flow in detail, with a fix for each failure.
- Claude Code users: the `regia-*` skills in `.claude/skills/` run these steps.
- Contributing needs the Contributor License Agreement: `CONTRIBUTING.md` and `CLA.md`.

## Uninstall

Nothing is installed outside the folders the user chose and the consents they gave. To remove Regia, delete the
engine folder (projects stay; they stop building without it). Undo `shell_path` by removing that line, and an MCP
entry by deleting it from the project's file. Rust (`rustup self uninstall`) and system packages are the user's to
remove; ask before touching them.
