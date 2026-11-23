# Onboarding: setting Regia up (agents)

This page is for an AI assistant that installs Regia Engine on a user's machine. It holds the setup flow in detail,
the question script with its ids and defaults, the consent steps, and a fix for each common failure. The rules are
in `AGENTS.md` at the root of the engine folder (section "Rules"); this page does not repeat them and never
overrides them. The user's view of the same flow is [Set up with your AI assistant](../start/assistant-setup.md),
and the hand install it automates is [Installing Regia Engine](../start/install.md).

Regia is built from source on the user's machine: there is no installer and no prebuilt download. You run the steps
of the install page yourself, one at a time, asking the user as you go.

## The setup flow

1. **Read first, download later.** Read `AGENTS.md` (over the web when you were given the repository URL,
   https://github.com/Regia-Engine/Regia-Engine). Naming the repository is consent to read that file, not to
   download the engine.
2. **Introduce** Regia in at most five sentences, name the three working modes, and say you will ask about six
   questions.
3. **Questions 1 and 2** (below), from the OS facts; no `regia` exists yet.
4. **Bootstrap** as `AGENTS.md` step 2 gives it, per OS: the prerequisites (the user's administrator commands),
   `rustup-init` checked against its `.sha256` (never a piped installer script), the clone or the release archive,
   `rustup toolchain install` in the engine folder (it reads `rust-toolchain.toml`), then
   `cargo build --release -p regia_cli -p regia_editor`.
5. **Check the machine** with `<engine>/target/release/regia doctor --platform <this OS> --json` and fix each
   failed check with its `fix` ([Toolchain doctor (agents)](doctor.md)); for a release archive, also
   `regia engine verify .`.
6. **Questions 3 to 6**, then create the project: `regia new`, a warm `cargo build -p <name>_launcher`, `regia run`
   once, `regia editor`.
7. **Modes 2 and 3:** write the project's MCP entry (below) and hand off to a new session in the project folder.
8. **The mode's opening**: mode 1, the editor tour ([Editor tour](../start/editor-tour.md)); mode 2, a screenshot
   of the editor through MCP and "what would you like to change?"; mode 3, "what game should we make?" and a short
   decision list.

Until a new shell has Regia on its `PATH`, call `regia` by its absolute path, `<engine>/target/release/regia`
(`regia.exe` on Windows).

## The question script

Ask in this order, each at most once per setup; skip a question whose answer the user already gave. Label every
question exactly as in the first column, and every consent `(Consent · <step id>)`.

| Label | Ask | When | Default |
|---|---|---|---|
| (Question 1 of 6 · machine) | "This looks like *Linux x86_64, 16 threads, 32 GB RAM, a discrete GPU with Vulkan, 400 GB free*. Set Regia up on this machine?" | always | yes |
| (Question 2 of 6 · install_from) | A git clone or the release archive (when the Releases page has one), and the engine folder | always | clone; Linux and macOS `~/Regia/regia-engine`, Windows `%LOCALAPPDATA%\Regia\e` |
| (Question 3 of 6 · targets) | Platforms to build games for; this machine always; Android, Windows from Linux and Steam Deck add downloads | always | this machine only |
| (Question 4 of 6 · mode) | The three modes, one line each; the user can switch any time | always | 1, local-first |
| (Question 4b · client) | Which assistant app the editor connection is for | modes 2 and 3, when you cannot tell | the app you run in |
| (Question 5 of 6 · template) | The templates (`regia new --help`), one line each, with a recommendation | always | `first_person` |
| (Question 6 of 6 · project) | Project name and projects folder | always | `My Game`, `~/RegiaProjects` |

A git clone is its own engine folder. On Windows keep the engine folder's path short: build paths under `target/`
are deep.

## Consents

Ask one consent per step, with what it downloads or writes, from where, and how large. The user may say yes to
several at once after seeing them together; that never covers an administrator command, which the user always runs
themselves.

| Step | Does | Size to say |
|---|---|---|
| `rust_toolchain` | downloads `rustup-init` from `static.rust-lang.org`, checks it, installs rustup for the user | about 15 MB, then the toolchain |
| `engine_source` | clones the repository or downloads the release archive; `rustup toolchain install` in it | up to about 1 GB of source; about 300 MB of toolchain downloads (1.3 GB installed) |
| `build_tools` | `cargo build --release -p regia_cli -p regia_editor`: crates from crates.io, then compiling | about 150 MB of crates (1.5 GB unpacked); 6 to 10 GB under `target/`; 20 to 60 minutes |
| `shell_path` | adds `<engine>/target/release` to `PATH` in one shell profile file | one line; name the file |
| `android_tools` | the Rust Android targets, `cargo-ndk`, the Android SDK, an NDK, a JDK 17+ and Gradle | about 3 GB; the SDK's licenses are the user's to accept |
| `cross_tools` | Windows from Linux (`cargo-xwin`, the MSVC target) or Steam Deck (`cargo-zigbuild`, zig) | a few hundred MB each |
| `warm_build` | `cargo build -p <name>_launcher` in the new project | a few more crates; minutes |
| `connect` | writes the `regia` MCP entry into the game project | one file in the project; name it |

The system packages of [Installing Regia Engine](../start/install.md) § 2 (headers, the Vulkan driver), the
C/C++ compiler and GPU drivers are administrator commands: show the exact command and why; the user runs it.

## Connecting (modes 2 and 3)

The MCP server is `regia mcp --project DIR`: one MCP tool per agent-protocol method, attached to the project's
running editor or game ([Agent protocol](../user-guide/agent-protocol.md)). With `--headless` it starts a
windowless editor when none answers and stops it when the server ends ([Headless editor](headless-editor.md)); with
`--http` it serves Streamable HTTP on a loopback port with a bearer token ([MCP over HTTP](mcp-http.md)).

For Claude Code, write `<project>/.mcp.json` in the shape of the template `dist/mcp/regia.mcp.json.example` (the
absolute path of `regia`, the absolute project path), keeping any other servers in that file. For any other client,
give the user the same command line for its MCP settings. Write nothing into the assistant's own user-wide settings.
A client reads the project's MCP file when a session starts in that folder: ask the user to start a new session
there and approve the `regia` server, then call `agent.hello` and take a screenshot. What the protocol may change is
set by the permission profiles in the project's `.regia/agent.toml`.

## Troubleshooting

Find the code in `regia doctor --json` (its `checks[].code` and `fix`) or the first error of the failed command,
then:

| Problem | What it means | Fix |
|---|---|---|
| `rustc_missing`, `cargo_missing` | Rust did not run | Install rustup (`rust_toolchain`); open a new shell or use `~/.cargo/bin` |
| `rustup_missing` | A distribution `rustc` without rustup | Desktop builds work when its version matches `rust-toolchain.toml`; Android and Windows cross builds need rustup |
| `vulkan_missing`, `vulkan_no_device` | No Vulkan driver, or no device | The user installs the Vulkan driver (`book/start/install.md` § 2) |
| `vulkaninfo_missing` | A driver is installed, `vulkaninfo` is not | The user installs `vulkan-tools`; not needed to build |
| `windows_target_missing`, `cargo_xwin_missing` | Windows cross build tools missing | `cross_tools`: the doctor prints the commands |
| `cargo_zigbuild_missing`, `zig_missing` | Steam Deck build tools missing | `cross_tools`: the doctor prints the commands |
| A `pkg-config` or linker error while building | System headers missing | The user installs the distribution row of `book/start/install.md` § 2 |
| `error: toolchain ... is not installed` | The pinned compiler is missing | `rustup toolchain install` in the engine folder |
| `No space left on device` | The build did not fit | Free space or choose another engine folder; a build needs about 40 GB |
| The build is killed with no error | Out of memory | Build with fewer jobs: `cargo build --release -j 2 -p regia_cli -p regia_editor` |
| `regia engine verify` reports errors | The release copy is damaged or changed | Download the archive again |
| `agent.hello` cannot connect | No editor or game runs in that project | Start `regia editor` or `regia run` there, or use `regia mcp --headless` |
| The `regia` server is not listed | The session started elsewhere, or the server was not approved | Start a new session in the project folder and approve it |

Every other doctor code is on [Toolchain doctor (agents)](doctor.md).

## Checks of these files

Two repository commands keep the onboarding files honest; both run in `cargo xtask preflight`.

`cargo xtask llms-txt [--check] [--out DIR]` writes `llms.txt` (the manual's index, in the layout of the public
llms.txt proposal) and `llms-full.txt` (the Start here, User guide, Systems and Agent guide pages in one file, each
after a `<!-- source: book/... -->` line, links rewritten to repository-root paths) into `dist/public-root/`.
`--check` writes nothing and fails when either file is stale, a linked page is missing, or a page has no `# ` title
or no first sentence.

`cargo xtask onboarding-lint` checks the user `AGENTS.md`, `CLAUDE.md`, `README.md`, the skills
(`dist/public-skills/`, published as `.claude/skills/`), the MCP template and this page against the onboarding
contract `xtask/onboarding/contract.md` (the question ids, consent steps, skills, prerequisites and the
`AGENTS.md` sections) and the `regia` command line. Its codes:

| Code | Meaning |
|---|---|
| `onboarding_file_missing` | A file the lint reads is missing |
| `question_id_mismatch` | A `Question N` label names the wrong id for N |
| `question_total_mismatch` | A label's `of N` is not the number of questions |
| `question_missing` | A file that must carry the whole script lacks a question |
| `question_order` | Questions first appear out of script order |
| `label_template_missing` | `AGENTS.md` does not state both label templates |
| `consent_step_unknown` | A `Consent · <step>` label names no step |
| `command_unknown` | A `regia` command in code is neither a command nor listed as pending |
| `pending_command_exists` | A command listed as pending exists now: remove it from the list |
| `agents_md_marker_missing` | The user `AGENTS.md` does not start with its marker line |
| `agents_md_too_long` | The user `AGENTS.md` has more than 200 lines |
| `agents_md_sections` | The user `AGENTS.md` sections differ from the contract's outline |
| `claude_md_differs` | `CLAUDE.md` is not the contract's text |
| `skill_missing`, `skill_unknown` | A contract skill is missing, or a skill folder is not in the contract |
| `skill_front_matter` | A skill's `name` differs from its folder, or its `description` is empty |
| `skill_too_long` | A skill body has more than 120 lines |
| `skill_no_agents_link` | A skill does not link the `AGENTS.md` section it runs |
| `skill_duplicates_rule` | A skill copies a rule of `AGENTS.md` instead of linking it |
| `mcp_template_invalid` | The MCP template is not the configuration shape |
| `prerequisites_differ` | The per-OS prerequisite table differs from the contract's |
| `install_row_missing` | A distribution the prerequisites name has no row in the install page |
| `link_missing` | A relative link of a user file resolves nowhere |
| `forbidden_term` | A user file contains a term from the private forbidden list |
