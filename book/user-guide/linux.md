# Linux

<!-- index: Linux -->
<!-- index: Vulkan -->
<!-- index: lavapipe -->
<!-- index: Wayland -->
<!-- index: X11 -->
<!-- index: xdg-open -->
<!-- index: Cursor grab -->

Linux is a development host and a shipping target. The editor, the command-line tool, games, and dedicated servers build and run here. The game renderer uses Vulkan. Install steps, package names, and the first `cargo build` live on [Installing Regia Engine](../start/install.md). This page is what the engine does on Linux after that install.

The [Steam Deck](steam-deck.md) runs the same native Linux package inside Steam Linux Runtime. [Windows](windows.md), [macOS (testing only)](macos.md), and [Android](android.md) are separate targets. [Packaging](packaging.md) and [Package a game](../how-to/platforms/package-a-game.md) cover the archives.

## Graphics

Vulkan is the Linux backend: Mesa's RADV or ANV, or the NVIDIA driver. There is no OpenGL fallback. Without a GPU, Mesa's lavapipe software driver runs headless tests. Golden images are recorded on lavapipe.

A desktop session needs a running display, or `xvfb-run` when there is none (a server or CI). `regia doctor --platform linux` checks that a Vulkan driver and `spirv-val` are present. `regia doctor --platform gpu` lists adapters and ray-tracing support and does not fail the build.

Vendor upscalers are not part of a Linux install. DLSS links when the optional `sdk` feature and an external SDK are both present. That SDK is not in the repository. Android and macOS do not link it. The quality page is [Automatic quality detection](auto-quality.md).

## Paths and platform services

`regia_platform::platform::linux` is the Linux service module.

| Service | What it does |
|---|---|
| Open a URL | `xdg-open`, started and not waited on |
| Locale | `LC_ALL`, then `LC_MESSAGES`, then `LANG` |
| OS information | `/etc/os-release` (or `/usr/lib/os-release`), the kernel release, DMI vendor and product, RAM from `/proc/meminfo` |
| Window name | Wayland `app_id` and X11 `WM_CLASS`, both set from the application's id |
| Clean-exit watchdog | `_exit`, so a stuck driver teardown cannot block process exit |

Dev cooks land in `.regia/cooked/linux` (`host_platform()` is `linux`).

| VFS | Host path |
|---|---|
| `user://` | `~/.local/share/RegiaEngine/<exe name>` |
| `cache://` | `~/.cache/RegiaEngine/<exe name>` |
| `logs://` | `user://logs` |

On Linux the local (non-roaming) data directory is the same folder as `user://`. The agent method `platform.paths` prints the paths of the machine you are on. Names Windows cannot store are refused on every platform, including Linux, so content authored here still packs for Windows. See [Windows](windows.md).

## Cursor grab

`CursorGrab::Locked` and `CursorGrab::Confined` both work on Wayland. On X11 and Xwayland, winit 0.30.13 does not implement `Locked`, so a Locked request is sent as Confined and logged once per process. Confined on X11 stays Confined. See `regia_platform::window_os`.

## Windows of the game

Spec 11: a `Window` component updates the operating-system window in `PostUpdate`. In this version only the primary window becomes an operating-system window (ADR-2252). Extra `Window` entities with no handle are skipped. Runtime games normally use one window. The same limit holds on Windows and macOS.

## What you can ship

`regia package --platform linux` writes `dist/linux/<slug>-linux.tar.gz` by default. `--format dir` leaves the staged directory for inspection. `--format appimage` needs `appimagetool` on `PATH` (or `REGIA_APPIMAGETOOL`); the tool is not bundled. A shipping profile links against glibc 2.31 so the binary starts in Steam Linux Runtime, including on a Deck. `--glibc host` keeps this machine's glibc and does not start on older distributions.

`--container steam-sniper` or `--container debian12` compiles that binary inside a pinned toolchain image. The file you ship is still the tar, the directory, or the AppImage. It is not an image of the game. Details, error codes, and the dedicated-server package are on [Packaging](packaging.md).

A dedicated server is `regia package --server --platform linux`. Android and macOS refuse that flag with `server_platform_unsupported` (exit code 2).

From Linux, `regia package --platform windows` cross-compiles with cargo-xwin when `regia doctor --platform windows` is clean. The resulting exe is not run on the Linux host. Android packages are built from Linux as well; [Android device bring-up](android-bringup.md) is the phone-side procedure.

## Check the machine

```sh
regia doctor --platform linux
regia doctor --verify-paks dist/linux/<slug>/content
```

Doctor exit codes, the Android and Deck checks, and `--verify-paks` are on [Checking your machine](doctor.md). Warnings do not fail the command. A failed check does.

## What this version does not do

- No OpenGL renderer, and no general web game. The browser program is the [product viewer](../agents/web-player.md) (WebGPU and WebGL2), which is a different target.
- No OpenXR loader. The simulator runs; `OpenXr` and `Auto` stay off with `xr_no_loader`. See [XR](../agents/xr.md).
- No macOS or iOS package from this host. macOS is a testing build. An iOS release is parked.
- No console package.

Licenses written into every package are on [Licensing](licensing.md).
