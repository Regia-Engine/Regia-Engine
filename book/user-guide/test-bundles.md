# Test bundles for Windows and the Steam Deck

`cargo xtask dist` builds a folder (and a zip or tar of it) that you copy to a Windows PC or a
Steam Deck to run engine binaries there, with no Rust toolchain on that machine. It is for engine
testing: the owner's Windows pass, the Deck test run on [Steam Deck](steam-deck.md), a tester's
crash report. A game ships with `regia package` instead ([Packaging](packaging.md)).

A bundle holds:

- the binaries, `vfx_lab` by default;
- Windows: a `run-<bin>.cmd` launcher per binary, the DLL set (`--dlls`) and the DXC pair for
  Direct3D 12 (`--dxc`), all next to the exe;
- the engine license and the third-party notices, in `licenses/`;
- optional pre-built pipeline caches, in `content/pipelines/`;
- a `README.txt` that says how to run it and which folders to send back;
- `dist-manifest.json`, which lists every file with its size and BLAKE3 hash.

## Commands

```console
$ cargo xtask dist --target windows --dry-run      # print the plan as JSON; builds and writes nothing
$ cargo xtask dist --target windows --dry-run --manifest-out windows-preview.json   # plus the manifest it would write
$ cargo xtask dist --target windows                # cargo-xwin cross build on Linux, then the zip
$ cargo xtask dist --target deck                   # cargo-zigbuild against glibc 2.31, then the tar
$ cargo xtask dist --verify target/dist/windows/regia-windows-0.1.27.zip
```

| Option | What it does |
|---|---|
| `--target windows\|deck` | The platform. Required, except with `--verify`. |
| `--bin <package>[:<bin>]` | A binary to bundle. Repeat it for more than one, for example `--bin vfx_lab --bin lookdev:do27_chase`. The default is `vfx_lab`. |
| `--profile <name>` | The cargo profile: `release` (the default), `shipping`, `dev` or a custom one. |
| `--out DIR` | The output folder. The default is `target/dist/<target>`. |
| `--from DIR` | Skip the build and bundle binaries that were already built into `DIR`. This works on any host. |
| `--pipeline-caches DIR` | Ship the `.fpc` files in `DIR`. Each file is validated and stored under its key's shipped name. When two files have the same key, the larger one is kept. A corrupt file stops the bundle. |
| `--crt-static` | Windows only: link the C runtime statically, so the exe does not need the Visual C++ Redistributable. It has not been tried with every C dependency yet. If the link fails, drop the flag. |
| `--dlls DIR` | Windows only: copy every `*.dll` of `DIR` next to the exe (for example `steam_api64.dll`). A DLL the exe imports and the bundle ships is classed `bundled`, so the import check passes. Each must be an x64 image. |
| `--dxc DIR` | Windows only: copy `dxcompiler.dll` and `dxil.dll` from `DIR` next to the exe. Both are required (`dxil.dll` signs what `dxcompiler.dll` compiles). See [Direct3D 12 and DXC](#direct3d-12-and-dxc). |
| `--no-launcher` | Windows only: do not write `run-<bin>.cmd`. |
| `--git REV` | The commit the binaries were built from, recorded as the manifest's `git` and in the README. The default is `REGIA_GIT_HASH`, then the repository's HEAD; give it with `--from` when the binaries were built from another commit (ADR-5002). A blank id or one with spaces fails with `dist_git_invalid`. |
| `--readme FILE` | Ship `FILE` as `README.txt` instead of the generated tester README. `demo-package` passes its player README here. |
| `--notices FILE` | Ship `FILE` as `licenses/THIRD_PARTY_NOTICES.md` instead of the engine-wide notices. `demo-package` passes the demo's own list (ADR-5419). |
| `--no-default-features`, `--features LIST` | Passed to the cargo build (ignored with `--from`). `demo-package` builds `--no-default-features --features demo`. |
| `--overlay DIR` | Ship every file under `DIR` at the same relative path in the bundle (role `overlay`). A path another plan file already holds fails the plan; `*.sh` files and files with an executable bit keep it. `demo-package` passes the cooked scene and the launchers here. |
| `--no-archive` | Write the folder only. |
| `--dry-run` | Print the plan and stop. |
| `--manifest-out FILE` | With `--dry-run` only: also write the manifest the bundle would get (see [The dry-run manifest](#the-dry-run-manifest)). |
| `--verify PATH` | Check a bundle folder, a `.zip` or a `.tar` against the manifest inside it, then exit. |

Build tools:

- **Windows on Linux:** `cargo install cargo-xwin`. The build runs
  `cargo xwin build --profile <p> --target x86_64-pc-windows-msvc -p <package> --bin <bin>`.
  On Windows, plain `cargo build` is used.
- **The Deck:** `cargo install cargo-zigbuild` and zig 0.14 or newer on `PATH`. The build runs
  `cargo zigbuild --profile <p> --target x86_64-unknown-linux-gnu.2.31`, which uses the glibc of
  Steam Linux Runtime 3.0. A Deck bundle builds on Linux only. On another host, build there and
  pass `--from`.

## Layout

```text
target/dist/<target>/
  regia-<target>-<version>/          the bundle folder
    vfx_lab.exe (Windows) or vfx_lab (Deck)
    run-vfx_lab.cmd                    Windows launcher, one per binary (not with --no-launcher)
    steam_api64.dll ...                only with --dlls
    dxcompiler.dll, dxil.dll           only with --dxc
    licenses/LICENSE                   the engine's MPL-2.0 text (README.txt names the source location)
    licenses/THIRD_PARTY_NOTICES.md
    content/pipelines/*.fpc            only with --pipeline-caches
    README.txt
    dist-manifest.json
  regia-<target>-<version>.zip|.tar  the same tree under one top-level folder
  symbols/vfx_lab.pdb                  Windows, when the linker wrote one (not in the archive)
```

The archives are written by xtask itself: a stored (uncompressed) zip for Windows, made by the same
writer as feedback bundles, and a ustar tar for the Deck. The tar keeps the executable bit. Neither
needs `zip`, `tar` or PowerShell on the build machine. The same files give the same archive bytes when `SOURCE_DATE_EPOCH` is set;
without it, the archive timestamp is the build time.

## The launcher

On Windows each binary gets `run-<bin>.cmd` (CRLF line ends). It changes to the bundle folder
wherever it was unzipped, sets `REGIA_NATIVE_CRASH=1` unless the variable is set already, runs the
exe with the arguments it was given, and on a non-zero exit code prints the code and waits for a
key, so a double-clicked window does not vanish with the error. The exit code is passed on.

## Direct3D 12 and DXC

The engine tries Vulkan first and falls back to Direct3D 12 (ADR-201). On Direct3D 12 the shader
compiler is one of three, and the manifest's `dx12` object says which this bundle relies on:

| `dx12.compiler` | Meaning |
|---|---|
| `bundled_dxc` | `--dxc` was given. `dxcompiler.dll` and `dxil.dll` sit next to the exe and shaders compile with DXC (Shader Model 6). Keep the two together. |
| `engine` | No DXC files. The exe uses the DXC linked into it when it was built with the `static-dxc` feature, otherwise FXC (Shader Model 5.1). |

The DXC files are not imports of the exe (they are loaded at run time), so the import check does
not need them. The Windows SDK ships a matching pair; take both from the same release.

## Symbols

The linker's `.pdb` is not in the zip. It is copied to `symbols/<bin>.pdb` next to the archive
and listed under `symbols[]` with its size and hash, so a minidump from a tester can be matched to
the exact build.

## The binary checks

`dist` reads the staged binaries before it writes the manifest. A failed check stops the bundle,
leaves the folder without a manifest for you to inspect, and the next run replaces it.

**Windows.** The exe's DLL imports are read from its PE header, so this check also runs on the Linux
machine that cross-built it. Each DLL falls into one class:

| Class | Meaning |
|---|---|
| `system` | Part of Windows 10 and 11, including the `api-ms-win-*` API sets and the Universal CRT. |
| `msvc_runtime` | `VCRUNTIME140.dll` and the other Visual C++ runtime DLLs. Most gaming PCs have the redistributable; a fresh Windows install does not. The README says so, and the manifest records `"msvc_runtime": "dynamic"`. `--crt-static` removes the dependency. |
| `gpu_driver` | `vulkan-1.dll`, which the GPU driver installs. |
| `bundled` | A DLL that ships next to the exe in the bundle. |
| `unknown` | Anything else. An unknown import **fails the bundle**, because the exe would not start without that DLL. An unknown delay-loaded DLL is only a note. |

If an `unknown` DLL is in fact part of Windows, add it to `WINDOWS_SYSTEM_DLLS` in
`xtask/src/dist_binary.rs`.

**The Deck.** The ELF's `NEEDED` libraries and the `GLIBC_x.y` versions it requires are read from
its section headers. A binary that needs a glibc newer than 2.31 **fails the bundle**. A library
that the Steam Linux Runtime does not provide is a note: the binary still runs in Desktop Mode
when SteamOS has that library. The runtime's library list is the one `regia package` uses, and a
test keeps the two lists equal.

## Where the program writes

The manifest's `runtime_dirs` and the README list these folders. `<exe>` is the program name
without `.exe`. When something goes wrong, the tester sends back `crash_dumps`, `logs` and the
run's `--out` folder.

| Key | Windows | Steam Deck |
|---|---|---|
| `user` | `%APPDATA%\RegiaEngine\<exe>` | `~/.local/share/RegiaEngine/<exe>` |
| `logs` | `%APPDATA%\RegiaEngine\<exe>\logs` | `~/.local/share/RegiaEngine/<exe>/logs` |
| `crash_dumps` | `%LOCALAPPDATA%\RegiaEngine\<exe>\feedback` | `~/.local/share/RegiaEngine/<exe>/feedback` |
| `cache` | `%LOCALAPPDATA%\RegiaEngine\<exe>` | `~/.cache/RegiaEngine/<exe>` |
| `pipeline_cache` | `%LOCALAPPDATA%\RegiaEngine\<exe>\pipelines` (Vulkan only; Direct3D 12 has no pipeline cache in wgpu 30) | `~/.cache/RegiaEngine/<exe>/pipelines` |
| `shipped_pipeline_caches` | `<bundle>\content\pipelines` | `<bundle>/content/pipelines` |

Notes:

- Native crash capture is opt-in on Windows. Set `REGIA_NATIVE_CRASH=1` before a test run; the
  README shows the command.
- `REGIA_PIPELINE_CACHE_DIR` moves the pipeline cache for one run.

## The manifest

`dist-manifest.json` (schema 1) has these fields:

| Field | What |
|---|---|
| `schema`, `kind` | `1` and `regia_dist`. `--verify` refuses other values. |
| `bundle`, `target`, `triple`, `engine_version`, `git`, `profile` | What was built, and from which revision. |
| `build` | `{tool, command}` (`cargo-xwin`, `cargo-zigbuild` or `cargo`), or `{tool: "prebuilt", from}` with `--from`. |
| `glibc_floor` | `"2.31"` for the Deck, `null` for Windows. |
| `binaries[]` | The binary checks. Windows: `machine`, `subsystem` (`console` or `windows`), `imports[] {dll, class, delay_load}`, `msvc_runtime`. Deck: `machine`, `needed`, `glibc_max`, `glibc_floor`, `outside_runtime`. |
| `dx12` | Windows: `{compiler, files, note}`, see [Direct3D 12 and DXC](#direct3d-12-and-dxc). `null` for the Deck. |
| `files[]` | `{path, role, bytes, blake3, mode}` for every file except the manifest, sorted by path. `role` is one of `binary`, `license`, `readme`, `pipeline_cache`, `dll`, `dxc`, `launcher` or `overlay`. `mode` is `755` or `644`. |
| `licenses` | The license files. |
| `runtime_dirs` | The table above, as `{key: {path, what}}`. |
| `symbols[]` | The `.pdb` files kept next to the bundle, with size and hash. |
| `warnings` | The notes, which the README repeats. |

`--verify` checks the following and lists every problem it finds:

- every listed file is present, with the listed size and hash;
- no file is present that the manifest does not list;
- the Deck binaries are executable;
- the manifest names at least one license;
- for a folder, the binary checks pass again.

## The dry-run manifest

`--dry-run --manifest-out FILE` writes, on any Linux machine and without a build, the manifest a
real bundle of the same plan would get. Use it to review a Windows zip layout before cargo-xwin
runs, or to diff two layouts in review. It has every field of the table above, plus:

| Field | What |
|---|---|
| `dry_run` | `true`. `--verify` refuses a manifest with this field, so a preview is never mistaken for a bundle. |
| `missing` | The bundle paths whose source does not exist yet (usually the binaries). |
| `archive` | The file name the zip or tar would get, or `null` with `--no-archive`. |

Files whose source exists already (the license files) carry their real size and hash, the same
values the staged bundle gets. Binaries not built yet carry `null`. The README hash is the README
without notes: binary checks can add notes (the Visual C++ runtime note, for example), and then
the staged README differs. `binaries[]` and `warnings` stay empty, because the binary checks need
the built files.

The Windows bundle is an installer-less folder. Unzip it anywhere outside `Program Files` and run
the exe; nothing is registered and nothing is written next to the exe. The license and the
third-party notices are always in `licenses\` (`cargo xtask notices` regenerates
`THIRD_PARTY_NOTICES.md`, and the gate checks that it is current). A game's own Windows package
comes from `regia package --platform windows` ([Packaging](packaging.md)), which also puts the
notices next to the exe.

Related: [Windows](windows.md), [Steam Deck](steam-deck.md), [Packaging](packaging.md).
