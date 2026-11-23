# Packaging the demo

`cargo xtask demo-package` turns the showcase's free-camera demo (`showcase --demo`) into something you can hand out or upload. It builds the `shipping` profile (`panic = "abort"`) for Windows and Linux, adds the cooked scene, the credits and the notices, and lays out a Steam depot and plain archives with checksums. It uploads nothing and reads no credential.

```text
cargo xtask demo-package --scene samples/showcase/example_scene --app-id 1234560
```

| Option | Meaning |
| --- | --- |
| `--scene DIR` | The scene to ship (default the generated example scene). It needs an `asset_manifest.json`. |
| `--out DIR` | Output folder (default `target/demo-package`). |
| `--targets windows,linux` | Platforms (default both). Linux is the glibc 2.31 build that also runs on the Steam Deck. |
| `--bin PKG[:BIN]` | The binary to ship, `<package>` or `<package>:<bin>` (default `showcase`). |
| `--app-id N` | Steam app id for the scripts. Without it they carry `REPLACE_WITH_APP_ID`. |
| `--from-windows DIR`, `--from-linux DIR` | Use binaries built elsewhere. |
| `--git-windows REV`, `--git-linux REV` | The commit the `--from-*` binaries were built from; required with them (ADR-5002). Recorded in the bundle manifest, the player README and `demo-package.json`. |
| `--check` | Only run the content and profile checks. |
| `--verify DIR` | Re-check a finished package. |

## What fails the build

The checks run before anything compiles, and every finding is printed:

- `do27_content_path`: a scene path, file name, manifest row or the scene directory's own path names Direct Order 27 content.
- `asset_no_licence`: a scene file has no row in `asset_manifest.json`, or the row's licence is empty, `unknown`, `TBD` or `TODO`.
- `asset_manifest_missing`, `asset_manifest_invalid`, `asset_row_duplicate`, `asset_row_orphan`, `scene_special_file`.
- `shipping_not_abort`: `[profile.shipping]` in the workspace does not set `panic = "abort"`.
- `demo_package_git_unknown`: `--from-windows` / `--from-linux` without `--git-windows` / `--git-linux`, or no git id at all (not a git checkout and no `REGIA_GIT_HASH`). `dist_git_invalid`: a given id is blank or has spaces.

## What you get

```text
depot/content/windows/ , depot/content/linux/   the bundle folders
depot/scripts/                                   app_build_<appid>.vdf, depot_build_<id>.vdf, UPLOAD.txt
archive/                                         regia-demo-<version>-windows.zip, -linux.tar, CHECKSUMS.b3
bundles/<platform>/                              the xtask dist output (dist-manifest.json inside)
DECK-CHECK.md                                    the Steam Deck check steps
WINDOWS-CHECK.md                                 the Windows minimum-spec check steps (ADR-5578)
demo-package.json                                what was built
```

Each bundle holds the binary, `run-demo.sh` or `run-demo.cmd`, `content/scene/`, `CREDITS.txt`, `licenses/`, `dist-manifest.json` and `README.txt`. The binary is `showcase --no-default-features --features demo` (ADR-5419): no agent protocol and no network crate; `dist` gets `--no-default-features --features demo`, and binaries given with `--from-windows`/`--from-linux` must be built the same way (`cargo xtask demo-notices --verify <depot>` refuses a binary with traces of a forbidden crate). `licenses/` holds `LICENSE` (the engine's MPL-2.0 text; `CREDITS.txt` names where the engine's source is), the demo's own `THIRD_PARTY_NOTICES.md` (a copy of `docs/demo/THIRD_PARTY_NOTICES-demo.md`, given to `dist --notices`) and `EULA.md` (`docs/demo/EULA.md` once the owner's text lands, the draft `docs/demo/EULA-DRAFT.md` until then); `demo-package` stops before building without them (`demo_package_legal_missing`), and `--verify` reports a folder without them (`demo_package_legal`). The bundle is what players install, so it holds no tester files (ADR-5002): `README.txt` is the player README (the demo's name and version, `Build <git>`, how to start it with and without Steam, the controls as the demo's panel shows them, what it does and does not send, and where it writes), and the Windows bundle has no `run-showcase.cmd` (`dist --no-launcher`). The texts are written to `readme/<platform>/README.txt` and passed to `dist --readme`. Check `archive/` with `b3sum --check CHECKSUMS.b3`, or `cargo xtask demo-package --verify target/demo-package`.

## Uploading (your step)

`depot/scripts/UPLOAD.txt` has the command: `steamcmd +login <build account> +run_app_build <path>/depot/scripts/app_build_<appid>.vdf`. `setlive` is empty, so the build waits on the partner site until you set a branch.

## Building the platforms

Windows builds with `cargo xwin` and Linux with `cargo zigbuild` (glibc 2.31) from a Linux host, as for [test bundles](test-bundles.md). Without them, build elsewhere and pass `--from-windows` / `--from-linux`.

The glibc floor also applies to linked system libraries. A newer host's ALSA,
udev or D-Bus library can prevent a 2.31 build from linking. Use a verified,
isolated build sysroot with compatible versions of those existing dependencies
and set `PKG_CONFIG_SYSROOT_DIR` and `PKG_CONFIG_LIBDIR` for the build. Keep these
build libraries out of the package. Do not suppress unresolved-symbol checks or
raise the floor to make a build pass; inspect the resulting ELF and test it in
the target runtime (ADR-4862).

## Complete-package verification

Verification requires `demo-package.json` and every target it names. Each target
needs its shipping-profile bundle, depot content directory, and archive; their
manifests must agree, and the files must match those manifests. The archive
inventory and `CHECKSUMS.b3` must cover exactly the named targets. Empty or
duplicate checksum lists fail. Shipped scene licenses are checked again, and
depot scripts must match the package targets with `setlive` empty (ADR-4862).
Links and special files are refused before opening package artifacts.

Additional diagnostics: `demo_package_invalid` for a missing/invalid summary or
bundle contract; `demo_package_inventory` for missing/unlisted files or checksum
rows; `demo_package_manifest` for differing manifests across copies;
`demo_package_script` for missing/changed depot scripts;
`demo_package_special_file` for links or special files;
`demo_package_git_unknown` for a bundle that records no build id;
`demo_package_git_mismatch` for a bundle whose id differs from the one `demo-package.json` records;
`demo_package_extra_launcher` for a `run-*.cmd` other than `run-demo.cmd`;
`demo_package_readme` for a `README.txt` that is not the player README of the bundle's build. An unreadable artifact
also fails verification. Existing dist and asset-license errors remain errors.

Checksums establish integrity against the provided manifests, not publisher
identity. Keep the separately recorded archive hashes with the handoff. A fixture
package in a unit test is not evidence of a runnable shipping build. Native
Windows/Linux launch, real gamepad input, and Deck acceptance use the exact
archives produced for release. The Linux tar extracts to `regia-deck-<version>/`.
