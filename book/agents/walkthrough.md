# Game-developer walkthrough (agents)

`cargo xtask walkthrough` runs the book's getting-started path the way a stranger would, and writes
a JSON report an agent can read. It is repository tooling for the engine team (ADR-2954), not a
`regia` subcommand, and it is a **rehearsal**: it runs on an engine machine with that machine's
toolchain and crate caches. The outside-tester check of the v1.0 row (a person who has never seen the
repository, on a clean machine) is arranged by the owner.

## What it runs

The commands come from the book. A shell fence whose info string is `sh,walkthrough=<stage>`
(optionally `,template=<id>`) in one of these pages, read in this order, is extracted and run
literally by `bash`:

1. [Installing Regia Engine](../start/install.md)
2. [Your first project](../getting-started.md)
3. [Running and iterating](../start/running.md)
4. [Editor tour](../start/editor-tour.md)
5. [Quick start](../user-guide/quick-start.md)
6. [Tutorial: a third-person game](../tutorials/third-person.md)
7. [Tutorial: the arena](../tutorials/arena.md)
8. [Build a networked game](../tutorials/networked-game.md)

The only substitution is `<version>` (the engine version on the archive). A fence tagged with a
template runs for that template only and replaces the untagged fences of its stage for it.

| Stage | Source | Runs |
|---|---|---|
| `engine-package` | harness | `regia engine package --allow-dirty` of the checkout, archived as `regia-engine-<version>.tar.gz` in the stranger's `~/Downloads` |
| `install` | install page | unpack the archive, `cargo build --release -p regia_cli -p regia_editor`, `export PATH` |
| `verify` | install page | `regia engine verify .` |
| `doctor` | install page | `regia doctor` |
| `new` | getting started, quick start, tutorials | `regia new` for every template (each template is a new terminal in `~`) |
| `smoke` | running, quick start, networked game | the game runs 5 frames headless and exits (`--exit-after-frames 5`) |
| `cook` | running | `regia cook` |
| `package` | getting started, quick start | `regia package --platform linux` (`cargo build --release` for `quick`) |
| `run-package` | harness | unpack `dist/linux/*-linux.tar.gz` elsewhere and run its binary with `--exit-after-frames 5` from `~` |
| `upgrade` | running | `regia upgrade-project`; any changed project file fails the stage |
| `android` | running | `cargo ndk -t arm64-v8a check -p my_game_android` (the `blank` project) |

Each template keeps one terminal session: the environment and directory a stage leaves are what its
next stage starts with. The stranger's environment has a temporary `HOME`, no `REGIA_*` or
`FERRITE_*` variables and no repository path on `PATH`; `CARGO_HOME`, `RUSTUP_HOME`, the display, <!-- rename-keep -->
the Vulkan driver choice, the Android SDK variables and the cargo job limits come from the host.
The generated projects share one `CARGO_TARGET_DIR`.

```console
$ xvfb-run -a cargo xtask walkthrough                  # everything, every template
$ cargo xtask walkthrough --list                       # the commands each stage would run
$ xvfb-run -a cargo xtask walkthrough --keep --template blank
$ xvfb-run -a cargo xtask walkthrough --stage smoke --template blank   # rerun one stage on a kept run
```

`--regia PATH` packages the engine with an already built `regia` (the default `cargo run -p regia_cli` can rebuild the engine under the CLI's own feature set: 39 min on a loaded the build server). `report.json` is rewritten after every stage, so a stopped run keeps what it finished. `--work DIR` moves the work directory (default `regia-walkthrough` under the system temp
directory; it may not be inside the repository). Without `--keep` or `--stage` it is deleted at the
end. Exit code 0 when every stage passed, 1 otherwise.

## The report

`target/walkthrough/report.json` (schema 1), with one log per stage at
`target/walkthrough/<stage>[-<template>].log`:

| Field | Meaning |
|---|---|
| `schema`, `kind` | `1`, `"regia_walkthrough"` |
| `ok` | Every stage reported has `status` `ok` |
| `note` | States that this is a rehearsal, not the outside-tester proof |
| `engine_version`, `work`, `templates` | The archive's version, the work directory, the templates walked |
| `differences_from_clean_machine` | Every way this run is easier than a clean machine (shared target directory, host caches, …) |
| `substitutions` | `[["<version>", "0.2.14"]]` |
| `stages[]` | `stage`, `template`, `status` (`ok`, `failed`, `timeout`, `skipped`), `sources` (`book/<page> (fence #N)` or `harness`), `commands`, `seconds`, `log`, `detail` |
| `findings[]` | The reading pass: `page`, `problem`, `fix` |
| `questions_for_owner[]` | Product questions the run surfaced |

A run with `--stage` merges its stages into the existing report. A walkthrough fence that names an
unknown stage or template, a fence for a harness stage, a stage with no fence, or a template the book
never creates stops the run before anything starts; the xtask unit test
`the_book_has_a_fence_for_every_stage_and_template` checks the same on every test run.
