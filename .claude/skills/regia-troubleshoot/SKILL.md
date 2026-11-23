---
name: regia-troubleshoot
description: Diagnose a failed Regia install, build, editor start or MCP connection.
---

# Troubleshoot Regia

Keep to the "Rules" section of `AGENTS.md` while fixing anything: system changes are the user's to run, downloads
need their consent, and the fix comes from Regia's own output, not from guesses. The table of failures with their
fixes is § Troubleshooting of `book/agents/onboarding.md`; the doctor's codes are in `book/agents/doctor.md`.

## 1. Collect the facts

Call `regia` by its absolute path if a new shell has not picked up `PATH` yet.

```sh
regia doctor --json                       # toolchains, Vulkan, platform tools; exit 1 on a failed check
regia doctor --platform android --json    # one platform (linux, windows, android, macos, deck, gpu, all)
regia engine info --json                  # this engine's version; in a project, its requirement and pin
```

For a game or editor that started and then failed, read the end of the terminal that ran `regia run` or
`regia editor`, and the crash report of the run (`book/agents/crash-reports.md` says where it is). Do not paste
the user's paths or hardware details anywhere outside this conversation.

## 2. Find the cause

- A doctor check with status `fail` or `warn`: its `code` and `fix` are in the JSON.
- A build error: the first `error[...]` or `error:` line of the `cargo` output, and which crate it names. A
  linker or `pkg-config` error on Linux almost always means a missing row of `book/start/install.md` § 2.
- An MCP problem: `regia agent call agent.hello` in the project tells whether the app is reachable at all.

## 3. Fix

- **Command needs administrator rights** (system packages, drivers, build tools): show the exact command and why;
  the user runs it; then run the doctor again.
- **Download or clone failed**: run it again once; twice in a row, tell the user and stop.
- **Not enough disk**: say which volume and how much is missing (a build needs about 40 GB); ask the user to free
  space or choose another folder. `regia clean --all` in a project frees its `target/`.
- **No GPU or only a software rasterizer** (`vulkan_missing`, `vulkan_no_device`): the user installs the vendor
  driver; then run `regia doctor` again.
- **Wrong compiler** (a `rust-toolchain.toml` error, or a distribution `rustc` of another version): run
  `rustup toolchain install` in the engine folder.
- **Editor connection** (no `regia` server listed, or `agent.hello` cannot connect): check that the editor or game
  runs in that project, that the MCP file is valid JSON with absolute paths, and that the session was started in
  the project folder and the user approved the server.
- **Anything else**: report the code or error line, the fix you would try and what you already tried, and wait
  for the user.

## 4. Confirm

Run the check that failed again (the doctor, the build, `agent.hello`) and say what changed. Resume the setup
with `regia-setup` when it was interrupted.
