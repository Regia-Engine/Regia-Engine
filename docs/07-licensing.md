# 07 — Licensing model

> **Not legal advice.** The owner's licence decision of 2026-11-20 (ADR-10225, which supersedes ADR-006 and the
> royalty terms of ADR-007). Counsel reviews the texts before the public release.

## 1. Owner decisions (2026-11-20, final)

| Part | Licence | File |
|---|---|---|
| Engine code (crates, editor, tools, xtask, shaders) | **Mozilla Public License 2.0** (`MPL-2.0`) | `LICENSE` (the verbatim MPL text) |
| Templates, samples and starter content: code | **MIT No Attribution** (`MIT-0`) | `crates/regia_templates/templates/LICENSE-MIT-0.txt`, `samples/LICENSE-MIT-0.txt` |
| Templates, samples and starter content: assets | **CC0 1.0 Universal** (`CC0-1.0`) | `.../LICENSE-CC0-1.0.txt` beside the code licence |
| Documentation and the book | **Creative Commons Attribution 4.0** (`CC-BY-4.0`) | `book/LICENSE-CC-BY-4.0.txt` |
| Outside contributions | A contributor licence agreement (CLA): contributors keep their copyright and grant the licensor the right to relicense | `CLA.md`, `CONTRIBUTING.md` |
| The names "Regia Engine" and the logo | Trademark policy, not a copyright licence | `TRADEMARKS.md` |

The licensor is **Sphenisca Regia LLC** (owner decision 2026-11-20): it holds the copyright, is the party to the
CLA and owns the trademark. The same licence applies to the sister projects (outside this repository). The public
repository is **https://github.com/Regia-Engine/Regia-Engine**, a fresh snapshot made by `cargo xtask public-export`
(ADR-10229), never this history.

There is no royalty, no revenue report, no release notice, no licence acceptance step and no licensee list:
anyone may use, change and ship the engine under the MPL.

Every first-party `.rs` and `.wgsl` file starts with its SPDX line: `// SPDX-License-Identifier: MPL-2.0` in the engine,
`MIT-0` in `samples/` and the templates, and the upstream licence's id in a file ported from another project (listed in
`docs/ported-sources.toml`, which keeps its attribution header). `cargo xtask spdx --check` runs in preflight and CI;
`--write` adds missing lines. `vendor/` and test inputs under `tests/**/fixtures/` are out of scope.

## 2. Why MPL-2.0

- **File-level copyleft.** The MPL applies file by file (MPL § 1.10, § 3.3). A game's own files, a studio's
  tools and a private console-platform module written as separate files can stay closed, under any licence,
  even when they are linked into one executable with Regia Engine (a "Larger Work", MPL § 3.3).
- **Changes come back.** A change to one of Regia Engine's own files is under the MPL, and whoever ships it
  must make that file's source available (MPL § 3.1, § 3.2). Improvements made in forks can be merged back.
- **Relicensing stays possible.** With the CLA, the licensor can offer future versions under other terms or
  sell editions (for example a supported or console edition) without asking every contributor again.
- **Compatible.** MPL-2.0 code can be combined with the permissive dependencies Regia uses (03 § 4). Regia's
  files are not marked "Incompatible With Secondary Licenses", so the MPL's secondary-licence clause applies as
  written (MPL § 3.3).

## 3. What the MPL asks of a game developer

| You do | What the MPL asks |
|---|---|
| Write your own game code, content, tools | Nothing. Your files are yours, under your terms, open or closed. |
| Ship a game built on unmodified Regia Engine | Tell players where Regia Engine's source code is (MPL § 3.2(a)). `regia package` writes that line for you (`REGIA-ENGINE-NOTICE.txt`, § 4 below). |
| Change one of Regia Engine's files and ship it | The changed files stay under the MPL; publish their source and point the notice at it (`[package] engine_source_url`). |
| Copy code from a template or sample | Nothing: MIT-0, no attribution needed. Template and sample assets are CC0. |
| Quote or adapt the book | Credit it (CC-BY-4.0). |

The executable itself may be under the game's own terms (MPL § 3.2(b)), as long as those terms do not limit
anyone's rights to the Regia Engine source.

## 4. What `regia package` writes (ADR-10225)

Every package (Linux, Windows, Android, dedicated server) carries:

- `REGIA-ENGINE-NOTICE.txt`: the MPL notice, the engine version and the source-location line. The location is
  `[package] engine_source_url` in `Regia.toml`, or the engine's own setting, the public repository
  `https://github.com/Regia-Engine/Regia-Engine`. A project that still sets the old placeholder `<release host>`
  (the "not set" value) gets a warning naming the key when it builds a shipping (release) package.
- `REGIA-ENGINE-LICENSE.txt`: the verbatim MPL-2.0 text.
- `CREDITS.txt`: the project's own `CREDITS.txt` (if any), then the line **"Made with Regia Engine"**. The
  credit is a default, **not a licence condition**: `regia package --no-engine-credit` or
  `[package] engine_credit = false` leaves it out.
- `THIRD_PARTY_NOTICES.md` (§ 6 below).

The engine test bundles (`cargo xtask dist`) and the demo package ship `licenses/LICENSE` and name the source
location in their `README.txt` / `CREDITS.txt`.

## 5. Trademarks

The licence does not grant trademark rights (MPL § 2.3). `TRADEMARKS.md`: "Made with Regia Engine" credits and
redistribution of unmodified releases may use the name; a modified distribution shipped as a product must use
another name; no use may suggest endorsement by the licensor.

## 6. Third-party components inside Regia Engine
Regia Engine's licence (MPL-2.0) covers only Regia Engine's own code. Dependencies keep their licences (MIT, Apache-2.0, BSD,
zlib, MPL-2.0, OFL fonts); `THIRD_PARTY_NOTICES.md` ships with every game build (generated in M16). Source
files ported from permissively licensed engines keep their notices and are listed in its "Ported source
code" section, generated from `docs/ported-sources.toml` (ADR-086). The
Steamworks SDK stays under Valve's agreement and is present only in Steam builds.

Native media codec libraries (ADR-1593) live in `regia_media` (built; `regia` features `media` and `media-codecs`, off by default): AV1 through `rav1d`, Opus through `unsafe-libopus` and Vorbis through `lewton` (ADR-1869). They are permissively licensed. `cargo xtask notices` now includes the dependency trees reached through `regia/media-codecs` and the opt-in `regia/media-codecs-asm` feature, including native build dependencies. The vendored, patched lewton 0.10.2 distribution (ADR-2164; patches in `vendor/lewton/REGIA-PATCHES.md`) is attributed through `docs/ported-sources.toml`. The generated `THIRD_PARTY_NOTICES.md` and its CLI mirror therefore cover these optional codecs too. The AV1 patent grant is a patent licence with defensive termination, separate from the code licence, and counsel reviews it.

Audit tooling (status, ADR-2760): `cargo xtask sbom` writes a CycloneDX 1.5 bill of materials (crates with licences and checksums, ported sources, the asset inventory) for the `engine` and `game` profiles. `docs/asset-licences.toml` records the origin and licence of every tracked binary asset, and `cargo xtask notices --check` (a step of `cargo xtask preflight`) fails on a binary file without a row; rows whose licence is `unknown` are listed as warnings until the owner settles them (`--strict` fails on them). A shipping third-party asset also needs a `[[notice]]` row, which puts its copyright lines and licence text (the full SIL OFL 1.1 text for fonts) into the notices; the check fails without one (ADR-8888). Method, schedule and readers are in `docs/03-workspace-and-dependencies.md` § 4e.

## 7. Direct Order 27
DO27 launches on Steam **with Regia Engine**, so it owes no third-party engine royalty. Its old third-party-engine
shell stays a reference and free test vehicle only: don't sell, monetise or accept money for any build of that
shell (paid early access, paid alpha keys, crowdfunding tiers that grant access), because that engine's own terms
would then apply. Free alpha distribution to testers creates no revenue.

### CLA bot switch (ADR-10254)

The CLA bot (`.github/workflows/cla.yml`) runs only when the repository variable **`CLA_ENABLED`** is exactly
`true` (GitHub: Settings > Secrets and variables > Actions > Variables). Unset, or any other value, skips its job:
no signature is recorded and no comment is posted. It stays off until the owner confirms that Sphenisca Regia LLC
is registered and counsel has reviewed `CLA.md`. Before setting it, create the signatures branch `cla-signatures`
and the secret `CLA_BOT_TOKEN` (contents:write on that branch). Setting the variable to anything else, or deleting
it, switches the bot off again.

## 8. Open questions (owner and counsel)
1. **Licensor registration:** the CLA bot stays off (`CLA_ENABLED` unset, § 4 "CLA bot switch") until the owner
   confirms Sphenisca Regia LLC is registered.
2. **Release host:** decided 2026-11-20: https://github.com/Regia-Engine/Regia-Engine (the default source location
   in every package notice and bundle README).
3. **Trademark registration** of "Regia Engine" and the logo before public use.
4. **Codec patents and licences:** H.264, HEVC and AAC patent pools, and the AV1 patent grant, before any shipped
   video or audio playback (ADR-1593); AV1, VP9, Opus and Vorbis are the defaults.

## 9. DLSS-enabled distributions (ADR-2002, ADR-4758)

The owner approved the NVIDIA SDK license on 2026-10-12; SDK v310.9.16 is the
2026-11-08 verification input. DLSS remains optional. No NVIDIA SDK file, runtime,
or generated binding belongs in the engine repository.

A DLSS Super Resolution game package needs the following **in addition to**
Regia's normal `THIRD_PARTY_NOTICES.md`:

| Item | Source in the approved SDK | Destination / requirement |
|---|---|---|
| Linux x86_64 release runtime | `lib/Linux_x86_64/rel/libnvidia-ngx-dlss.so.310.9.16` | Beside the executable; preserve the versioned filename |
| Windows x86_64 release runtime | `lib/Windows_x86_64/rel/nvngx_dlss.dll` | Beside the executable; preserve NVIDIA's signature |
| NVIDIA terms | `LICENSE.txt` | Include as `NVIDIA-DLSS-LICENSE.txt`; retain proprietary notices |
| Complete runtime third-party notices | Matching `doc/DLSS_Programming_Guide_Release.pdf`, third-party license section | Include the complete applicable copyright/license texts in public product documentation, for example `NVIDIA-DLSS-THIRD-PARTY-NOTICES.txt` |

Use `rel`, never `dev`; do not include headers, static link libraries, debug
overlays, sample tools, or the unrelated Ray Reconstruction / Frame Generation
runtimes. Uninstallation must remove the runtime. Signing must preserve NVIDIA's
signature. These rules come from the SDK guide § 4.2. Its § 4.2.3 refers to § 9.5;
the supplied March 31, 2026 guide's third-party licenses are actually in § 9.6.
Review the section title and complete texts, not just that stale number.

The [NVIDIA RTX SDK license](https://github.com/NVIDIA/DLSS/blob/main/LICENSE.txt)
requires protective downstream terms, pre-commercial-release notification, and
SDK attribution/marks on the applicable splash, about and game-credit surfaces.
Its supplement governs trademark review and other distribution obligations.
Copying a runtime and text files does not perform those product/release steps.
The owner handles the notification and approved marks; this lane sends nothing
and does not treat technical acceptance as commercial-release approval.

The packager has an explicit staging opt-in (four synthetic-file tests pass on Linux):
`REGIA_PACKAGE_DLSS=1`, `DLSS_SDK=/absolute/approved/sdk`, and
`REGIA_DLSS_NOTICES=/absolute/complete-notices.txt`. It validates these files
before cooking/building and copies only the target release SR runtime and the
two text files. This does not enable the game's Cargo `dlss` feature or prove NGX
linkage. Without the opt-in it stages no NVIDIA files. Validate the staged
application with `REGIA_DLSS_PATH` unset, so loading must find the runtime beside
the executable, and record the runtime and notice hashes in release evidence.
Android and macOS continue to use the SDK-free stub; they ship no NGX runtime.

XeSS-enabled Windows distributions (ADR-4656, ADR-7820) have the same shape: the
SDK stays outside the repository, and nothing from it is staged without an
explicit opt-in. `REGIA_PACKAGE_XESS=1` with `XESS_SDK=/absolute/approved/sdk`
makes `regia package --platform windows` copy Intel's unmodified `bin/libxess.dll`
beside the executable with the SDK's `LICENSE.txt` (Intel Simplified Software
License, October 2022) and `third-party-programs.txt` as `INTEL-XESS-LICENSE.txt`
and `INTEL-XESS-THIRD-PARTY-PROGRAMS.txt`; `cargo xtask demo-package --xess-sdk
<SDK>` does the same for the demo's Windows bundle and adds the XeSS section of the
engine notices to that bundle's notices. Other SDK DLLs (frame generation, low
latency, DX11) are never staged. Counsel confirms the licence text before a build
that ships the runtime is released; that decision stays with the owner (ADR-7695).
