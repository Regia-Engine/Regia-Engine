# Onboarding contract (v0.6.x)

What `cargo xtask onboarding-lint` checks the user onboarding files against on the v0.6 line (ADR-10230). The v0.6
line has no `regia setup`: the assistant runs the steps of `book/start/install.md` itself, asking as it goes. Spec
125 (`docs/specs/125-agent-onboarding.md`, the v0.8 guided setup) replaces this file when its commands land. The
section numbers match spec 125's, so the lint reads either.

### 4.2 `CLAUDE.md`

```markdown
@AGENTS.md

Claude Code: the project skills in `.claude/skills/` (regia-setup, regia-new-project, regia-connect-editor,
regia-troubleshoot) run the steps of AGENTS.md. Use them when the user asks to set up, start a project, connect
to the editor or fix a problem.
```

### 5.1 Skills

Sources under `dist/public-skills/<name>/SKILL.md`; the export writes them to `.claude/skills/<name>/SKILL.md`.

| Skill | Description (trigger) |
|---|---|
| `regia-setup` | Set up Regia on this machine: questions, prerequisites, Rust, clone, build, doctor |
| `regia-new-project` | Create a project from a template, warm build, run once, open the editor |
| `regia-connect-editor` | Write the project's MCP entry (modes 2 and 3) and hand off to a new session |
| `regia-troubleshoot` | Diagnose a failed install, build, editor start or MCP connection |

### 6.1 Prerequisites

| OS | Prerequisites before the first build |
|---|---|
| Linux | `build-essential` (Debian/Ubuntu), `gcc gcc-c++ make` (Fedora), `base-devel` (Arch), the distribution's row of `book/start/install.md` § 2, `git` for a clone |
| Windows | Visual Studio Build Tools with the C++ workload before `rustup-init`; `git` for a clone |
| macOS | `xcode-select --install` (also provides `git`) |

### 6.3 Steps that need a consent

| Id | Does |
|---|---|
| `rust_toolchain` | `rustup-init`, checked against its `.sha256`, run as the user |
| `engine_source` | the git clone or the release archive; `rustup toolchain install` in it |
| `build_tools` | `cargo build --release -p regia_cli -p regia_editor` (crates.io downloads, the build) |
| `shell_path` | one `PATH` line in one shell profile file |
| `android_tools` | the Android targets, `cargo-ndk`, SDK, NDK, JDK and Gradle |
| `cross_tools` | Windows-from-Linux or Steam Deck build tools |
| `warm_build` | the new project's first `cargo build` |
| `connect` | the `regia` MCP entry in the game project |

## 7. The question script

| # | Id | Ask |
|---|---|---|
| Q1 | `machine` | set Regia up on this machine |
| Q2 | `install_from` | git clone or release archive, and the engine folder |
| Q3 | `targets` | platforms to build games for |
| Q4 | `mode` | local-first, co-pilot or assistant builds |
| Q4b | `client` | which assistant app the connection is for (modes 2 and 3) |
| Q5 | `template` | the first project's template |
| Q6 | `project` | project name and projects folder |

## Appendix A. The user `AGENTS.md` outline

The sections of the user `AGENTS.md` in order, and the labels its setup section carries.

````markdown
<!-- regia:user-agents-md -->
# Regia Engine: instructions for AI assistants

## Rules

Label questions `(Question N of 6 · <id>)` and consents `(Consent · <step id>)`.

## Setting Regia up

(Question 1 of 6 · machine) (Question 2 of 6 · install_from) `regia doctor --json`
(Question 3 of 6 · targets) (Question 4 of 6 · mode) (Question 4b · client)
(Question 5 of 6 · template) `regia new --help` (Question 6 of 6 · project) `regia new "My Game"`

## Working modes

## Everyday commands

## More

## Uninstall
````
