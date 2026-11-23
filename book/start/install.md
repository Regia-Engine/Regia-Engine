# Installing Regia Engine

> **Rather let an AI assistant do it?** Open the engine folder in an assistant that can run commands and say
> *"Set Regia up for me."* It follows the same steps and asks you before every download and every administrator
> command: [Set up with your AI assistant](assistant-setup.md).

Regia Engine is open source (MPL-2.0, see [Licensing](../user-guide/licensing.md)) and distributed as source. You build the command-line tool and the editor
from your engine checkout, and every project you create path-depends on that checkout. This
chapter sets up a machine for desktop development and, optionally, for Android.

## 0. Get the engine

An engine release arrives as one archive, `regia-engine-<version>.tar.gz` (made with
`regia engine package`, [Distribution and upgrades](../agents/distribution-upgrade.md)). Unpack it
where it will stay, because every project you create refers to that folder by path, and work inside
it for the rest of this chapter. In a terminal, in the folder that holds the archive:

```sh,walkthrough=install
tar -xzf regia-engine-<version>.tar.gz
cd regia-engine-<version>
```

Replace `<version>` with the version in the archive's name. If you work from a git checkout of the
engine instead, `cd` into the checkout; everything below is the same.

## 1. Rust

The engine pins one stable compiler in `rust-toolchain.toml` at the repository root (currently
Rust 1.99.0, with `rustfmt`, `clippy` and `rust-src`, and the Linux, Windows and Android targets).
Install Rust with [rustup](https://rustup.rs): it reads that file and fetches the right toolchain
the first time you run `cargo` inside the checkout. Rustup also adds the Android targets later.
A distribution `rustc` builds the desktop targets if its version matches, but cannot add
Android targets.

## 2. A GPU driver with Vulkan (or DX12 on Windows)

The renderer uses Vulkan on Linux and Android, and DX12 or Vulkan on Windows. It never falls back
to OpenGL. On Linux, install your distribution's Vulkan driver (Mesa's RADV/ANV, or the NVIDIA
driver) and `vulkan-tools` so `vulkaninfo` can confirm it. Without a GPU, Mesa's `lavapipe`
software driver runs every headless test, slowly; it is what the engine's golden images are
recorded on.

On Linux the build also needs `pkg-config` and the development headers of ALSA, udev, D-Bus,
xkbcommon and Wayland (the same set the engine's CI installs):

| Distribution | Command |
|---|---|
| Debian, Ubuntu | `sudo apt install pkg-config libasound2-dev libudev-dev libdbus-1-dev libxkbcommon-dev libwayland-dev mesa-vulkan-drivers vulkan-tools` |
| Fedora | `sudo dnf install pkgconf-pkg-config alsa-lib-devel systemd-devel dbus-devel libxkbcommon-devel wayland-devel mesa-vulkan-drivers vulkan-tools` |
| Arch | `sudo pacman -S pkgconf alsa-lib systemd-libs dbus libxkbcommon wayland vulkan-icd-loader vulkan-tools` |

To run a game without a display (a server, CI), also install `xvfb` and start it under `xvfb-run`.

## 3. Build the tools

From the engine folder:

```sh,walkthrough=install
cargo build --release -p regia_cli -p regia_editor
export PATH="$PWD/target/release:$PATH"
```

The first build takes a while (the whole engine, optimized). The `export` line only lasts for this
terminal: add it to your shell profile (`~/.bashrc`, `~/.zshrc`) with the absolute path of the
engine folder in place of `$PWD`, so every new terminal finds `regia`. If you set
`CARGO_TARGET_DIR`, the programs are in `$CARGO_TARGET_DIR/release` instead of `target/release`.

On Windows, in PowerShell, the same steps are (Windows 10 and later include `tar`):

```powershell
tar -xzf regia-engine-<version>.tar.gz
cd regia-engine-<version>
cargo build --release -p regia_cli -p regia_editor
$env:Path = "$PWD\target\release;$env:Path"
```

and the folder goes into the user `Path` (Settings → System → About → Advanced system settings →
Environment Variables) to last.

This produces the two programs you use (plus `regia-hot-host`, a scripted hot-reload host the
engine's own tests drive, and `regia-ext-host`, a scripted extension host for the dynamic-extension
tests):

| Program | What it is |
|---|---|
| `regia` | The command-line tool: create projects, cook, package, preview assets, run the MCP server, call the agent protocol ([CLI reference](../reference/cli.md)) |
| `regia-editor` | The editor ([Editor tour](editor-tour.md)) |

Check that the engine folder is complete and unmodified against the release manifest
(`regia-engine.release.ron`; a git checkout has none, so skip this there):

```sh,walkthrough=verify
regia engine verify .
```

It reports a missing or changed file as an error and an extra file (your own notes, say) as a
warning; `target/` is ignored.

## 4. Check the machine

For desktop development on Linux (on Windows, `--platform windows`):

```sh,walkthrough=doctor
regia doctor --platform linux
```

Plain `regia doctor` also checks the Android tools of § 5, so on a machine without them it reports
those as failures; run it once you have set up Android.

`regia doctor` checks Rust, cargo, rustup, `cargo-ndk`, Vulkan and, for Android, the SDK, the
NDK, a JDK 17+, Gradle, `adb` and the Rust Android targets. Each failed check prints the command
that fixes it. `--platform linux|windows|android|macos|deck|gpu|all` narrows or widens the check
(`windows` on Linux checks the cross-build tools: cargo-xwin, the MSVC target, clang-cl; `deck` checks
cargo-zigbuild and zig); `--project DIR` uses that project's `rust-toolchain.toml`; `--json` prints the checks
for scripts and agents ([Toolchain doctor](../agents/doctor.md)). Run it again after each fix until it reports no
failures.

## 5. Android (optional)

You only need this to ship on Android. The editor itself is desktop-only.

1. Add the Rust targets: `rustup target add aarch64-linux-android x86_64-linux-android`.
2. Install the Android SDK command-line tools and set `ANDROID_HOME`; install an NDK (r27 or
   newer) with `sdkmanager` and set `ANDROID_NDK_HOME` if it is not under the SDK.
3. Install a JDK 17+ and Gradle (the packager generates a small Gradle project around your game's
   `cdylib`).
4. `cargo install cargo-ndk`.
5. Connect a device with USB debugging on; `adb devices` should list it.

`regia doctor --platform android` verifies all of it. Packaging and installing on a device are
covered in [Packaging](../user-guide/packaging.md).

## 6. Windows notes

Windows builds need the MSVC toolchain (Visual Studio Build Tools with the C++ workload). The
engine is developed on Linux and verified on Windows in a virtual machine; both are supported
targets, and every change is built for Windows.

## 7. Optional tools

| Tool | Used for |
|---|---|
| `mdbook` | Building this manual as a multi-page website (`cargo xtask manual`) |
| Tracy | The `tracy` feature of `regia_core` streams profiling spans to it ([Profiling](../systems/profiling.md)) |
| `steamcmd` | `regia steam upload` ([Steam](../user-guide/steam.md)) |
| `jq` | Handy for scripting `regia agent call` output |

Next: [create your first project](../getting-started.md).
