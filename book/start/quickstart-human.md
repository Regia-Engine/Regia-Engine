# Quickstart for a person

<!-- index: Quickstart -->

Install the tools, create a project, and run it. The longer tour, with a moving object, the editor
and a screenshot, is [Your first project](../getting-started.md). Installing Rust, a GPU driver and
the system packages is [Installing Regia Engine](install.md). An assistant that can run commands
can follow [Set up with your AI assistant](assistant-setup.md) instead of typing these steps.

## 1. Build the tools

From the engine folder (the unpacked release, or a git checkout):

```console
$ cargo build --release -p regia_cli -p regia_editor
$ export PATH="$PWD/target/release:$PATH"
```

The first build compiles the engine. It takes a while. The `export` line lasts for this terminal.
To keep `regia` on your `PATH`, put the absolute path of `target/release` in your shell profile.
If `CARGO_TARGET_DIR` is set, the programs are in `$CARGO_TARGET_DIR/release`.

On Windows, in PowerShell, the path line is `$env:Path = "$PWD\target\release;$env:Path"`. The
rest of [Installing Regia Engine](install.md) covers the toolchain pin, Vulkan or DX12, and the
Linux packages.

## 2. Create a project

In the folder that should hold your projects (not inside the engine folder):

```console
$ regia new "My Game"
$ cd my_game
```

`regia new` writes a Cargo workspace in `my_game/` and prints that path. The default template is
`blank`: an empty game plugin and a startup level with two entities. Other templates are
`--template first_person`, `third_person`, `top_down`, `quick` and `net-game`. The table and the
naming rules are in [Your first project](../getting-started.md).

## 3. Run it

```console
$ regia run
```

`regia run` cooks the project's assets and starts the game in a window. Leave that terminal alone
until you close the window. The command returns when the game exits.

To open the same project in the editor, from the parent of `my_game`:

```console
$ regia-editor --project my_game
```

## Finished check

The game window opens on the startup level. Closing the window ends `regia run` with status 0.
If the window never appears, use [Troubleshooting](../manual/troubleshooting.md) and
[Checking your machine (doctor)](../user-guide/doctor.md).

## Where next

- [Your first project](../getting-started.md): put something on screen, move it, open the editor,
  take a screenshot.
- [Editor tour](editor-tour.md) and [Running and iterating](running.md).
- [Feature catalog](../manual/catalog.md): whether a feature is in this version.
