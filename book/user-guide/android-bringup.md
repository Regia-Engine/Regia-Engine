# Android device bring-up

A step-by-step guide from a bare machine to a game running on a phone, with the agent protocol
attached. Every command here exists in `regia` (`regia_cli`), `cargo xtask` or `regia doctor`; the
last section lists the messages those tools print and what to do about each. What the engine does on
Android, the thermal governor and the long pass are on the [Android](android.md) page.

Commands starting with `regia` run from the installed CLI. In an engine checkout use
`cargo run -p regia_cli --` in front of the arguments. `cargo xtask` always runs from the engine
checkout.

## 0. What you need

| Item | Notes |
|---|---|
| A phone with USB debugging on | Android 10 (API 29) or newer with Vulkan. `min_sdk` in `Regia.toml` defaults to 29 and the manifest check refuses less than 24. |
| A USB cable that carries data | Charge-only cables show no device in `adb devices`. |
| A Linux, macOS or Windows host | Packaging is verified from Linux. |
| About 6 GB of disk | SDK, NDK, Gradle caches and the Rust Android targets. |

## 1. Install the toolchain

1. Rust targets (rustup-managed toolchain; a distro `rustc` without rustup cannot add them):

   ```sh
   rustup target add aarch64-linux-android x86_64-linux-android
   ```

2. Android SDK command-line tools, then the platform-tools, an API 35 platform (the default
   `target_sdk`) and an NDK r27 or newer. Set `ANDROID_HOME` to the SDK directory:

   ```sh
   sdkmanager "platform-tools" "platforms;android-35" "ndk;28.0.13004108"
   ```

   Without `ANDROID_HOME` or `ANDROID_SDK_ROOT`, `regia` looks in the Android Studio default
   (`~/Android/Sdk` on Linux, `~/Library/Android/sdk` on macOS, `%LOCALAPPDATA%\Android\Sdk` on
   Windows). Set `ANDROID_NDK_HOME` if the NDK is somewhere else; otherwise the newest folder under
   `<sdk>/ndk` is used.

3. JDK 17 or newer (`JAVA_HOME` or `java` on `PATH`) and Gradle 8.14.3. Gradle is found in this
   order: `REGIA_GRADLE` (path to the launcher), `gradle` on `PATH`, then a Gradle already cached
   under `~/.gradle/wrapper/dists` (or `GRADLE_USER_HOME`) with the same major version and at
   least 8.13.
4. `cargo-ndk`:

   ```sh
   cargo install cargo-ndk
   ```

5. Check it all:

   ```sh
   regia doctor --platform android --project <game>
   ```

   Run it again after each fix until no Android line says `FAIL` (each problem prints its `fix:`
   line under it) and the device count is at least 1 after step 2. `doctor` fails an NDK older than
   r27 (`android_ndk_too_old`) and a missing `cargo-ndk` (`cargo_ndk_missing`).

## 2. Prepare the phone

1. Settings, About phone: tap *Build number* seven times to enable Developer options.
2. Developer options: turn on *USB debugging*.
3. Plug in, unlock the phone, and accept the *Allow USB debugging?* prompt on the phone.
4. On the host:

   ```sh
   adb devices
   ```

   The phone must be listed with state `device`. `unauthorized` means the prompt was not accepted;
   `offline` means reconnect the cable or run `adb kill-server`.
5. With several devices attached, note the serial from that list and pass it as `--device <serial>`
   to every command below.

Hardened Android builds (GrapheneOS) leave the `INTERNET` permission off for adb installs.
`install-android` grants it for debug builds (`pm grant`); stock Android refuses that call, which is
fine, because it grants the permission at install.

## 3. Check the project

The game needs `[platforms.android]` in `Regia.toml` and an Android crate that builds a `cdylib`
named `main` (see `samples/sim_bridge` for the layout):

```toml
[platforms.android]
min_sdk = 29
target_sdk = 35
abis = ["arm64-v8a", "x86_64"]
cargo_package = "sim_bridge_android"   # the Android crate, when it is not named `android`
```

`abis` lists what a release build contains. A debug `install-android` builds only the device's own
ABI when the manifest lists it (so a phone builds `arm64-v8a` only); `--abi <abi>` (repeatable)
overrides that, and an ABI outside the manifest list is an error.

## 4. Build, install, launch

```sh
regia install-android --project <game> [--device <serial>]
```

In order, it builds the native library with `cargo ndk`, runs Gradle for a debug-signed APK, runs
`adb install -r`, grants `INTERNET`, starts `<id>/.RegiaActivity`, waits up to 15 s for the process,
waits up to 60 s for the app to advertise its agent server (the first launch compiles pipelines),
forwards a free host port to it with `adb forward`, writes `<project>/.regia/agent/endpoint.json`
and `token`, and then streams the app's logcat until Ctrl-C. `--no-logcat` returns after launch.

The first build is slow (a cold `cargo ndk` build plus Gradle downloads). To reinstall the last
package without rebuilding, use `--no-build`; the APK is `dist/android/<slug>-debug.apk`.

## 5. Talk to the running app

```sh
regia agent call agent.hello       # any agent method; see the agent protocol page
regia agent call render.get_profile
regia agent call seeing.screenshot --out shot.png
```

If the forward was lost (you restarted adb, unplugged the cable, or the app restarted), re-attach
without building or installing:

```sh
regia install-android --project <game> --attach
```

Files on the device: `user://` is the app's files directory and `logs://` is `user://logs`. In a
debug build pull them with:

```sh
adb exec-out run-as <id> tar c -C files logs | tar x
```

## 6. Check lifecycle, then run the full pass

The lifecycle check needs the APK installed (step 4). It sends the app home and back, rotates the
screen, and fails on a crash, a restart or PSS growth over 5 %:

```sh
cargo xtask android-lifecycle --package <id> [--device <serial>] [--cycles 20] [--rotations 10] \
    [--suspend-with screen] [--report lifecycle.json]
```

Use `--suspend-with screen` on a phone with a PIN: the HOME key on the lock screen does not
background the app. The tool turns *stay awake while plugged in* on for the run and restores it
afterwards.

The full pass (install, lifecycle, Mobile Low and High, optional DO27 map, 20-minute thermal run)
is documented on the [Android](android.md) page:

```sh
cargo xtask android-pass --package <id> --project <game> [--device <serial>] [--thermal-minutes 20]
```

Two things to know about `android-pass`:

- `--no-build` skips `install-android` entirely, including the agent attach. The pass attaches again
  after the lifecycle step, so `--no-build` works with the default lifecycle step. With
  `--no-build --cycles 0` nothing attaches: run `regia install-android --project <game> --attach`
  first.
- It finds `adb` through `ANDROID_HOME` or `ANDROID_SDK_ROOT`, then `PATH`. It does not look in the
  default SDK folder; if `adb` is not on `PATH`, export `ANDROID_HOME`.

## 7. Troubleshooting

Messages are quoted from the tools. Where a message ends in a command, run it.

| What you see | Cause | Fix |
|---|---|---|
| `Android SDK not found at ... (ANDROID_HOME)` | The variable points at a missing folder, or the default folder does not exist. | Install the SDK and set `ANDROID_HOME` to its directory. |
| `Android NDK not found (ANDROID_NDK_HOME unset, nothing in .../ndk)` | No NDK under the SDK. | `sdkmanager "ndk;28.0.13004108"` or set `ANDROID_NDK_HOME`. |
| `Android SDK platform N is not installed ...` | The `target_sdk` platform is missing. | `sdkmanager "platforms;android-N"`, or lower `target_sdk` in `Regia.toml`. |
| `Gradle needs JDK 17 or newer ...` or `cannot read the Java version` | Old or missing JDK. | Install JDK 17+ and set `JAVA_HOME`. |
| `Gradle not found` | No Gradle on `PATH` or in the wrapper cache. | Install Gradle 8.14.3, or set `REGIA_GRADLE` to the launcher. |
| `adb not found` | No platform-tools. | `sdkmanager platform-tools`, or put `adb` on `PATH`. |
| `FAIL <target>: rustup target not installed` | Missing Rust Android target. | `rustup target add <target>`. |
| `FAIL rustup: not found; distro rustc builds the desktop targets, ...` | Distro `rustc` without rustup. | Install rustup; Android targets need a rustup-managed toolchain. |
| `FAIL cargo-ndk: not found; Android builds run through it` | Not installed. | `cargo install cargo-ndk --locked`. |
| `building the Android native library` fails with a linker or `cc` error | NDK older than r27, or `ANDROID_NDK_HOME` points at another NDK. | Install a current NDK and point `ANDROID_NDK_HOME` at it. |
| `lib<main>.so was not produced` | The Android crate is not `[lib] name = "main"` with `crate-type = ["cdylib"]`, or `cargo_package` names the wrong crate. | Fix the crate, or set `cargo_package` under `[platforms.android]`. |
| `--abi X is not in [platforms.android] abis` | ABI not listed in the manifest. | Add it to `abis`. |
| `adb install failed (is a device attached and authorized?)` | No device, `unauthorized`, or a conflicting signature from another build. | Check `adb devices`; accept the prompt; if the signature differs, `adb uninstall <id>` and install again (this deletes the app's data). |
| `no Android device attached` | Cable, USB debugging, or adb server. | Replug, enable USB debugging, `adb kill-server`, check `adb devices`. |
| `device S is unauthorized` | USB debugging prompt not accepted. | Unlock the phone and accept the prompt. |
| `device S is not attached; attached: ...` | Wrong `--device` serial. | Use a serial from `adb devices`. |
| More than one device and the tool asks for a serial | Several devices attached. | Pass `--device <serial>`. |
| `<id> did not start within 15 s; it probably crashed on launch` | Native crash or missing library on start. | `adb logcat -d -b crash` and `adb logcat -d -s RustStdoutStderr GameActivity`. |
| `did not advertise an agent server within 60 s` | Shipping build without the `agent` feature, `INTERNET` denied, or a very slow first launch. | `adb logcat -s regia_agent`; on GrapheneOS enable *Network* for the app; build the dev profile; retry with `--attach` after the app is up. |
| `cannot read the app's agent token (is the APK debuggable?)` | A release-signed APK; `run-as` only works for debug builds. | Install the dev profile build (`regia package --platform android --format apk`). |
| `regia agent call` cannot connect after a replug | The `adb forward` is gone. | `regia install-android --project <game> --attach`. |
| `cannot reach the app's agent (is it running? ...)` from `android-pass` | The app is not running or the endpoint file is stale. | `regia install-android --project <game> --attach`, then run the pass again. |
| `<id> is not installed on S` from `android-lifecycle` | No APK on the device. | `regia install-android --project <game> --no-logcat`. |
| Lifecycle fails with a missing marker (`Suspended`/`Resumed`) | Locked phone, so HOME did not background the app. | Add `--suspend-with screen`. |
| `release signing requested but no [platforms.android] keystore` | `--profile shipping` without a keystore. | Set the `env:` keystore variables ([Packaging](packaging.md)), or use the dev profile. |
| Frame time rises over a long session | Thermal throttling; the governor lowers the render scale. | `render.get_profile` shows `thermal`; see [Android](android.md). |

## 8. Emulator

An x86_64 emulator image (API 34) works for lifecycle and agent checks: start it with
`emulator -avd <name>`, then use the same commands (`adb devices` lists it as `emulator-5554`). Its
frame times say nothing about a phone's.
