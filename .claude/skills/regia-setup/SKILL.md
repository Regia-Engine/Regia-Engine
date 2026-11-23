---
name: regia-setup
description: Set up Regia Engine on this machine: check hardware, ask the setup questions, install Rust and the engine, build, verify. Use when the user asks to install or set up Regia, or opens this repository for the first time.
---

# Set up Regia Engine

This skill runs "Setting Regia up" of `AGENTS.md`. The "Rules" section of `AGENTS.md` applies to every step here;
read it first and keep to it. The whole flow and a fix for each failure are in `book/agents/onboarding.md`; the
hand install it automates is `book/start/install.md`.

## 0. Resume or start

If an engine folder with a built `regia` already exists (`<engine>/target/release/regia`), run
`regia doctor --json` by its absolute path and continue at the first step that is not done (a failed check, the
project questions, the editor connection). Do not ask answered questions again. Otherwise start at 1.

## 1. Introduce, then the two questions before any download

1. Say what Regia is in at most five sentences (the first paragraph of `AGENTS.md`), that it runs on this
   machine and needs no account, the three working modes in one line, and that you will ask about six
   questions.
2. **(Question 1 of 6 · machine)** Before `regia` exists, read the facts from the OS:
   - Linux: `uname -srm`, `nproc`, `free -g`, `lspci | grep -i -E 'vga|3d'`, `df -h ~`
   - macOS: `sw_vers`, `sysctl -n machdep.cpu.brand_string hw.ncpu hw.memsize`, `system_profiler SPDisplaysDataType`
   - Windows (PowerShell): `Get-ComputerInfo -Property OsName,CsProcessors,CsTotalPhysicalMemory`,
     `Get-CimInstance Win32_VideoController | Select Name`, `Get-PSDrive C`

   Name the OS, CPU threads, RAM, GPU and free disk, and ask whether to set Regia up here.
3. **(Question 2 of 6 · install_from)** A git clone (default) or the release archive from the repository's Releases
   page when it has one for this version; both build from source. Ask for the engine folder with its default
   from `AGENTS.md`.

## 2. Bootstrap

Follow the bootstrap of `AGENTS.md` step 2 exactly, in its order:

1. The prerequisites row for this OS. Each one is the user's to run; wait until they say it is done.
2. **(Consent · rust_toolchain)** `rustup-init`, checked against its `.sha256`. Skip it when `rustup --version`
   already works.
3. **(Consent · engine_source)** the clone or the archive, with its size, then `rustup toolchain install` in the
   engine folder.
4. **(Consent · build_tools)** `cargo build --release -p regia_cli -p regia_editor` in the engine folder, with the
   download and disk sizes from `AGENTS.md`. Relay progress every few minutes; the first build is long.
5. Remember the absolute path of `regia` from here on: `<engine>/target/release/regia`.
6. Archive only: `regia engine verify .` in the engine folder; an error means a damaged or changed copy.
7. `regia doctor --platform <this OS> --json`. A `fail` check: show its `fix`; when it is an administrator
   command, the user runs it. Run the doctor again until no check fails. Open `regia-troubleshoot` when a fix is
   not a single command.
8. Offer **(Consent · shell_path)**: the exact profile file and the one line that puts `<engine>/target/release`
   on `PATH`.

## 3. The remaining questions

- **(Question 3 of 6 · targets)** This machine always; Android, Windows from Linux and Steam Deck each add
  downloads. Default: this machine only. For each one picked, run `regia doctor --platform <target> --json`, show
  what is missing with sizes, ask **(Consent · android_tools)** or **(Consent · cross_tools)**, and follow the
  fixes (Android: `book/start/install.md` § 5; the Android SDK's licenses are the user's to accept).
- **(Question 4 of 6 · mode)** The three modes of `AGENTS.md` § "Working modes", one line each, default 1, and
  that the user can switch later.
- **(Question 4b · client)** Only for modes 2 and 3, and only when you cannot tell which assistant app you run in.
- **(Question 5 of 6 · template)** The templates from `regia new --help`, one line each; recommend one from what
  the user said they want to make. Default `first_person`.
- **(Question 6 of 6 · project)** Project name (default `My Game`) and projects folder (default
  `~/RegiaProjects`).

## 4. Create and verify

Open `regia-new-project` with the answers: it creates the project, warms the build, runs it once and opens the
editor. When the game window opened, say so with the doctor's result.

## 5. Hand over

- Mode 1: offer the editor tour (`book/start/editor-tour.md`) and stop.
- Modes 2 and 3: open `regia-connect-editor`.
- A user who stopped early: say what is done and what is not; "continue Regia setup" resumes it from step 0.
