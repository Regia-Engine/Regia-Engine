# Troubleshooting

<!-- index: Troubleshooting -->

Checks for a setup that does not reach a running game or a connected agent. Platform limits that
are not bugs are on [Known issues and limits](known-issues.md). Installing the toolchain is
[Installing Regia Engine](../start/install.md).

## `regia` is not found

The `export PATH="$PWD/target/release:$PATH"` line in the quickstart lasts for one terminal, and
only after `cargo build --release -p regia_cli -p regia_editor` has finished in the engine
folder. A new terminal needs that directory on `PATH` again. If `CARGO_TARGET_DIR` is set, look
in `$CARGO_TARGET_DIR/release`, not `target/release`.

`regia new` refuses a name that starts with a digit, a Rust or Java reserved word, or a Windows
device name (`con`, `nul`, `com1`, `lpt1`). It also refuses a target folder that already has
files in it. [Your first project](../getting-started.md) has the full rule.

## The build fails before the game exists

The engine pins the compiler in `rust-toolchain.toml`. `cargo` inside the checkout should fetch
that toolchain. A system `rustc` of a different version will not match.

On Linux the build needs `pkg-config` and the development packages listed in
[Installing Regia Engine](../start/install.md) (ALSA, udev, D-Bus, xkbcommon, Wayland). A missing
package fails in a crate's build script; install the set and rebuild.

## The window never opens

The renderer uses Vulkan on Linux and Android, and DX12 or Vulkan on Windows. It does not fall
back to OpenGL. `regia doctor` reports what this machine can see
([Checking your machine](../user-guide/doctor.md)). Without a GPU, Mesa's lavapipe software
driver runs headless tests; it is not a stand-in for a game window.

`regia run` cooks, then starts the launcher. A cook error names the asset. Fix that file, or
see [Upgrade notes](upgrade.md) if the message is that the file's version is newer than the
engine or that a migrator failed.

## The agent cannot connect

`regia agent call` and `regia mcp --attach` read `<project>/.regia/agent/` from the project you
name. The game or the editor has to be running in that project. "Cannot connect" means nothing
is listening yet: wait and retry `regia agent call agent.hello`.

Headless: `regia editor --headless --project DIR` prints `{"event":"ready"}` when it listens.
`regia mcp --headless --project DIR` starts that editor if needed.
[Quickstart for an agent](../start/quickstart-agent.md).

For HTTP, take the URL and the bearer token from `regia mcp token`, not from a
`mcp_http.json` you opened by hand. The worked session is [MCP over HTTP](../agents/mcp-http.md).
A non-loopback bind needs `--allow-remote` and still wants TLS in front of it; the default is
`127.0.0.1`.

When a call returns an error, use `error.data.code` with [Errors](../agents/protocol.md#errors).
`F0012` is permission denied ([Safety](../agents/safety.md)). `F0008` is an unknown method.

## Content files and `upgrade-project`

A dry run (`regia upgrade-project`, no `--write`) only lists files. `--write` is the step that
rewrites them. If it fails, the project is unchanged and no `.bak` is left (ADR-8811). A
directory left behind by a killed run is described in [Upgrade notes](upgrade.md). Do not delete
a `.bak` until the new file loads.

## Headset, vendor upscalers, macOS

An OpenXR session does not start in this tree: `OpenXr` resolves to off with `xr_no_loader`.
The desktop game still runs. [Known issues and limits](known-issues.md).

DLSS and XeSS link only when their SDK is present and the crate feature is on. They are not in
the repository. A machine without the SDK uses the engine's own upscaler path.

macOS is a testing build on Metal. Do not use a macOS tree as the dedicated-server package.
