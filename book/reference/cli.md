# Command-line reference

Regia Engine has three command-line programs: `regia` (the project tool), `regia-editor`, and the
repository's `cargo xtask` automation. Most `regia` subcommands take `--project DIR` (default:
the current directory), the folder that holds `Regia.toml`; `regia new` creates that folder and
takes `--path DIR` (its parent) instead. `regia <command> --help` lists every flag of a command.
Log verbosity follows the `REGIA_LOG` environment variable (default `info`).

## `regia`

### `regia new <NAME>`

Create a project from a template.

| Option | Default | Meaning |
|---|---|---|
| `NAME` | (required) | Display name; the directory and crate names are its snake_case form |
| `--template ID` | `blank` | `blank`, `first_person`, `third_person`, `top_down`, `quick` (one crate, one `main.rs`) or `net-game` (also `net_game`: a dedicated server, a client and the shared game crate) |
| `--path DIR` | current directory | Parent directory to create the project in (there is no `--project` here) |
| `--engine-source SRC` | the engine checkout `regia` runs from (`REGIA_ENGINE_DIR`, or the checkout around the executable); outside any checkout, `git:https://github.com/Regia-Engine/Regia-Engine#v<version>` | Where the engine crates come from: `path:<dir>`, `git:<url>#<tag>` or `registry:<name>[@<version>]` |

### `regia doctor`

Check the toolchains and print a fix for each failure: Rust, cargo, rustup, `cargo-ndk`, Vulkan,
and for Android the SDK, NDK, JDK 17+, Gradle, `adb` and the Rust Android targets.

| Option | Meaning |
|---|---|
| `--platform P` | Check one platform: `linux`, `windows`, `android`, `macos`, `deck` or `all`; `gpu` runs the GPU ray-tracing probe alone (default: all that apply on this host) |
| `--project DIR` | Use that project's `rust-toolchain.toml` |
| `--json` | Print the checks as JSON ([`doctor` agent page](../agents/doctor.md)) instead of the text report |
| `--verify-paks DIR` | Verify the whole-file content hash of every `.fpak` in `DIR` (a shipped game's `content/`); only this check runs |

### `regia cook`

Cook the project's assets into `.regia/cooked/<platform>/` ([Assets and the cook](../fundamentals/assets.md)).

| Option | Default | Meaning |
|---|---|---|
| `--platform P` | host | `linux`, `windows` or `android` |
| `--quality fast\|final` | `final` | `fast` writes uncompressed paks and skips the shipping gate. `final` (the default) compresses paks and, before the cook, runs the provenance-policy gate and, when the manifest sets `require_review_for_shipping`, the unreviewed-asset gate. Either gate can fail the command before any pak is written. Texture compression is the same for both |
| `--clean` | off | Delete all cooked output and rebuild |
| `--stats` | off | Print the canonical reference terrain budget (4096×4096 heightfield, 8192×8192 macro, two masks, six 4096×4096 layers, two maps; not this project's terrain), then this project's cooked terrain cache and the cooked textures' formats, then exit |
| `--json` | off | With `--stats`: only the texture format report, as JSON. With `--size-report`: the size report as JSON |
| `--size-report` | off | Do not cook: measure every image as the desktop (BCn), Android (ASTC) and ETC2 cooks would write it |
| `--ship` | off | Write cooked packs (desktop meshes GPU-ready, and a blob lz4 saves less than 15 % of stored raw so block-compressed textures load as mapping views, ADR-7971) and fail when a pak entry would be transcoded or cannot load at runtime |
| `--gpu-meshes` | off | Store desktop pak meshes GPU-ready (as `REGIA_PAK_GPU_MESHES=1`) |
| `--pack-report` | off | Do not cook: inspect the platform's existing paks and print the cooked-pack report as JSON |
| `--shared-cache PATH` | the project's `[cook.shared_cache]` | A shared cook cache (directory, network share or `http://` cache server): fetch the cooked outputs it holds, publish what this cook imports ([Shared cook cache](../agents/cook-cache.md)) |
| `--read-only` | off | Use the shared cook cache for reading only |
| `--no-shared-cache` | off | Ignore the project's shared cook cache for this cook |

`regia cook cache prune --max-size SIZE` removes least-recently-used entries until the cache holds at most `SIZE`
(bytes, or a number with K, M, G or T, e.g. `500G`); `regia cook cache stats` prints its entry count and size and the
last cook's shared-cache summary. Both take `--shared-cache PATH` and `--json`.

### `regia clean`

Remove regenerable project state ([Packaging](../user-guide/packaging.md)).

| Option | Default | Meaning |
|---|---|---|
| (none) | | The cook and asset-database caches |
| `--cache` | off | Also the shader and pipeline caches under the project state directory |
| `--all` | off | Everything `--cache` removes, plus `build/`, `dist/` and `target/` |
| `--dry-run` | off | List what would be removed and remove nothing |
| `--project DIR` | `.` | Project directory (contains `Regia.toml`) |

### `regia preview <ASSET>`

Cook one asset if needed and show it in a window with a camera and a key light.

| Option | Default | Meaning |
|---|---|---|
| `ASSET` | (required) | Asset path, e.g. `models/DamagedHelmet.glb` |
| `--exit-after-frames N` | `0` | Quit after N frames (`0` runs until closed) |

### `regia package`

Cook, build the launcher and assemble a distributable ([Packaging](../user-guide/packaging.md)).

| Option | Default | Meaning |
|---|---|---|
| `--platform linux\|windows\|android` | (required) | Target platform |
| `--format apk\|aab\|dir\|tar\|zip` | `tar` (Linux), `zip` (Windows), `apk` (Android) | Output format; `dir` is a plain directory for inspection. `app` (macOS) is reserved and deferred |
| `--profile NAME` | `dev` (`server` with `--server`) | `dev`: cargo `dev` profile, debug-signed. `shipping`: optimized, stripped, release-signed, symbols written separately. Any `[profiles.<name>]` entry of `Regia.toml` also works: the package ships only that profile's levels and their dependencies |
| `--out DIR` | `<project>/dist/<platform>` | Output directory |
| `--steam` | off | Also stage the Steam depot layout and steamcmd VDFs under `<out>/steam` |
| `--server` | off | Build the dedicated server (no window, render or audio crate; `server.toml`, `bans.ron`). Linux and Windows only |
| `--dry-run` | off | Cook if needed, print the size report of what the profile ships, and stop |
| `--abi ABI` | every manifest ABI | Android only: build just these ABIs (repeatable) |
| `--glibc VERSION\|host` | `2.31` for shipping and `--steam` | Linux only: the newest glibc the game binary may require |
| `--pipeline-caches DIR` | `<project>/pipeline-caches` when present | Desktop only: pre-built pipeline caches (`.fpc`) to ship in `content/pipelines/` |

### `regia install-android`

Package, install with `adb`, launch, and stream the app's logcat.

| Option | Meaning |
|---|---|
| `--project DIR` | Project directory |
| `--device SERIAL` | Target device (`adb devices`); needed when several are attached |
| `--no-build` | Install the APK from the last `regia package` |
| `--no-logcat` | Return after launching |
| `--abi ABI` | ABI to build (repeatable; default: the device's primary ABI when the manifest lists it) |
| `--attach` | Only attach the agent protocol to the already running app (`adb forward` plus the host-side `.regia/agent/endpoint.json`): no build, install or launch |

### `regia upgrade-project`

Run the content-format migrators over the project ([Content formats](formats.md)). Without
`--write` it lists what would change; with `--write` it migrates files in place, writing a `.bak`
of each original first.

| Option | Meaning |
|---|---|
| `--write` | Migrate files in place |
| `--engine SRC` | Move the project's engine dependencies to `path:<dir>`, `git:<url>#<tag>` or `registry:<name>[@<version>]`; it must name this engine's version |
| `--json` | Print the plan (dry run) or the result (`--write`) as one JSON document |
| `--backup DIR` | With `--write`: first copy the project's source files (not `target/`, `build/`, `dist/`, `.git/`, the subfolders of `.regia/` or caches; the files directly in `.regia/`, such as the editor layouts `--write` migrates, are copied) to `DIR/<project>-backup-<UTC YYYYMMDD-HHMMSS>/`; `DIR` must be outside the project. The JSON document gains `backup { written, path, files, bytes, skipped }` |

### `regia net desync-replay <BUNDLE>`

Replay a lockstep desync bundle (`*.desync.ron`, `docs/formats/desync-ron.md`) headless through the `DesyncReplay` hook
registered for its game and print the first diverging tick, peer and subsystem, every peer's comparison, and whether
the live probe said the same ([Net desync (agents)](../agents/net-desync.md)). This binary registers the engine's sample
hook (`regia.desync_sample`); a game replays its own bundles from its tool binary with `ReplayRegistry`
([Networking](../user-guide/networking.md#desync-bundles)). Exit code 0 when it replayed, 1 when the bundle was
refused (another format version, another game or game version, no hook) or the hook failed.

| Option | Meaning |
|---|---|
| `--json` | Print one JSON object: the verdict with `ok: true`, or `ok: false` and `error: { code, message }` |

### `regia steam upload`

Upload the packaged Steam depots with `steamcmd` ([Steam](../user-guide/steam.md)). Credentials
come only from `STEAM_USERNAME`, `STEAM_PASSWORD` and `STEAM_TOTP`.

| Option | Meaning |
|---|---|
| `--branch NAME` | (required) A branch listed in `[steam] branches`, or `default` |
| `--live` | Allow publishing to `default`, the branch players get |
| `--yes` | Skip the `default`-branch confirmation (CI) |
| `--dry-run` | Regenerate the VDFs and print the steamcmd invocation without uploading |
| `--dist DIR` | Distribution root with the packaged depots (default `<project>/dist`) |

### `regia agent call <METHOD> [PARAMS_JSON]`

Call one agent-protocol method on the running app of this project and print the JSON result
([Agent protocol and MCP](../user-guide/agent-protocol.md)). `PARAMS_JSON` defaults to `{}`.
`--out FILE` writes a screenshot result to a PNG instead of printing base64. The app's endpoint
and token are read from `<project>/.regia/agent/`.

### `regia mcp`

Run an MCP server over stdio that attaches to the project's running app: one MCP tool per protocol
method that has a tool name, plus `regia://` resources. Point Claude Code or any MCP client at
`regia mcp --project DIR`. `--attach` attaches and never starts anything, which is also what no flag
does. `--headless` starts a headless editor when no app answers, and takes it down when the adapter
ends; see [Headless editor](../agents/headless-editor.md).

| Option | Meaning |
|---|---|
| `--headless` | When no app of the project answers, start `regia-editor --headless`, wait for its ready line and attach (`mcp_editor_start_failed`, `mcp_editor_exited`, `mcp_editor_ready_timeout`) |
| `--no-gpu` | With `--headless`: the started editor opens no GPU device |
| `--editor-bin FILE` | With `--headless`: the editor binary (default: `REGIA_EDITOR`, next to `regia`, then `PATH`) |
| `--editor-timeout SECS` | With `--headless`: how long to wait for the ready line, and for the editor to exit at the end (default 120) |
| `--http [ADDR]` | Serve MCP over Streamable HTTP instead of stdio (default `127.0.0.1` on a free port); the URL and token file are written to `.regia/agent/mcp_http.json` |
| `--allow-remote` | With `--http`: allow a non-loopback `ADDR` (the bearer token then guards it) |
| `--allow-origin ORIGIN` | With `--http`: also accept this `Origin` header (repeatable) |
| `--http-max-body BYTES` | With `--http`: the largest request body |
| `--token-file FILE` | With `--http`: read the bearer token from this file instead of minting one per launch |

### `regia run`

Cook the project, build its desktop launcher and run it ([Running](../start/running.md)).

| Option | Meaning |
|---|---|
| `--dev` | Live asset reload: saved sources are re-cooked in the background and reloaded in the game |
| `--level PATH` | Start in this level (`.level.ron` or `.level.d`, relative to the project or its `assets/`) |
| `--release` | Build with the release profile |
| `-- ARGS` | Arguments passed to the game |

### `regia server`

Build and run the game's dedicated server from the dev cache ([Dedicated servers](../user-guide/networking.md)).

| Option | Meaning |
|---|---|
| `--level PATH` | Serve this level (replaces the rotation) |
| `--port PORT` | UDP game port |
| `--clients N` | Also start N offscreen clients that join on loopback (0 to 64) |
| `--agent` | Start the agent protocol server in the server process (dev builds only) |

### `regia test`

Run the functional tests placed in a level (`regia test --level L`), or a multi-process
playthrough (`regia test session FILE`). The desktop game still opens its window
(`REGIA_SESSION_RENDER=0` is advisory, spec 41 § 2.8). Exit codes: 0 passed, 1 failed, 2 aborted or undecided,
3 no tests ran, 4 harness or launch failure. Main flags: `--level`, `--filter GLOB`, `--tag TAG`,
`--seed N`, `--param key=value`, `--report FILE` (JSON), `--junit FILE`, `--update-expected`,
`--loop`, `--timeout-s` (single-shot status budget in seconds, default 60; the ready wait keeps
its own 60 s), `--exe FILE`, `--platform desktop|android`.

### `regia engine`

Engine releases and the version policy: `regia engine info` (this engine; in a project, its
requirement and pin), `regia engine package` (a release directory with a file manifest),
`regia engine verify DIR` (check a release copy). Each takes `--json`.

### `regia bench`

Run the public benchmark scene set headless and print frame stats. `--list` shows the set,
`--fetch` downloads it with checksums and licences; `--frames`, `--warmup`, `--size WxH`,
`--preset`, `--on FEATURES`, `--json` and `--png-dir DIR` shape the run.

### `regia prepare`

Warm the shipped pipelines headless and write a stamp (stdout is JSON lines). `--check` reports
whether the stamp is current (exit 2 when a prepare is needed), `--status` prints the status file,
`--force` warms anyway; `--cache-dir`, `--shipped-dir`, `--salt`, `--profile`, `--threads` and
`--timeout-secs` configure it.

### `regia editor [PROJECT_DIR]`

Run the editor on a project. It finds the `regia-editor` binary (`--editor-bin`, then `REGIA_EDITOR`, then next to
this `regia`, then on `PATH`) and runs it; its exit status is this command's.

| Option | Meaning |
|---|---|
| `--project DIR` | The project directory, the same as `PROJECT_DIR` (default: the current directory) |
| `--headless` | No window: the edit world, the asset database, hot reload and the agent server. Prints one `{"event":"ready"}` JSON line on stdout when the agent server listens, and exits cleanly, saving, on `agent.shutdown`, SIGTERM or Ctrl+C |
| `--no-gpu` | With `--headless`: open no GPU device; view methods answer `editor_headless` |
| `--game CRATE_DIR` | Host this game crate for hot reload and play-in-editor |
| `--exit-after-frames N` | Exit after N frames (a headless editor saves first, like on SIGTERM) |
| `--editor-bin FILE` | The editor binary |

See [Headless editor](../agents/headless-editor.md).

### `regia release`

Verify, diff, or plan the upload of a directory of release chunks (ADR-8557). `regia release verify DIR`
checks manifests against the files (`--deep` also checks archive members; exit 1 when a check fails).
`regia release diff OLD NEW` prints chunks, moved files and download bytes (exit 0 even when the
releases differ). `regia release upload-plan DIR` prints an ordered JSON plan, chunks first and
manifests last, and does not contact a host. `--json` is schema 1.

### Other subcommands

`regia <command> --help` documents each one.

| Command | What it does |
|---|---|
| `regia render` | One image of a running app's view: the real-time frame, or `--reference` traced by the reference path tracer |
| `regia bake-gi` | Bake a diffuse GI probe volume (`.fgiv`) with the reference path tracer |
| `regia bake-lightmaps` | Bake a lightmap atlas (`.flmap`) for a scene's static meshes; `--inspect` reads one |
| `regia bake-captures` | Bake the open level's local reflection captures in the running app and save them to a `.fcap` named in the level; `--check FILE` reads one offline |
| `regia apk-size` | What a built APK or AAB is made of; `--json`, `--budget-mb` |
| `regia table` | Data tables without the game's row type: `validate`, `to-csv`, `to-ron` |
| `regia dcc-feed` | The DCC scene feed's reference client: stream a glTF file, record, replay, dump |
| `regia procgen` | Procedural graphs: `graph check`, `import-scatter` |
| `regia assets` | Query the cook's asset registry: `assets query`, `assets tags`; report and write a font subset: `assets font-subset`; content chunks: `assets chunks` (`--max-size SIZE`, `--max-total SIZE`, `--measure disk\|memory`: the size-budget gate, exit 1 over budget), `assets why`; signing: `assets keygen --out FILE`, `assets sign <manifest\|dir> --key FILE`; see [Signed content manifests](../agents/content-trust.md) |
| `regia content` | Content chunks: cook a DLC or patch chunk against a frozen base |
| `regia loc` | Localization: `gather`, `validate`, `pseudo`, `plural` |
| `regia atlas` | Unwrap and pack a `.obj` mesh into a lightmap UV atlas |
| `regia import` | Inspect an `.fbx` or `.obj` with its importer; `--out` writes the converted `.glb` |
| `regia fracture` | Pre-fracture a closed mesh into Voronoi chunks and cook the destruction chunk asset |
| `regia tiled-autotile` | Import the Wang sets and terrains of a Tiled `.tsx`, `.tsj`, `.tmx` or `.tmj` file as `.autotile.ron` sets; `--out-dir`, `--json`; see [2D](../user-guide/2d.md) |
| `regia plugin` | The project's extensions: `list`, `info`, `check`, `enable`, `disable`, `sync`, `build`, `new` |
| `regia level` | Convert a level between `.level.ron` and the directory form: `split`, `merge` |
| `regia trace` | `trace export`: a `.regiatrace` recording to JSON lines or the Trace Event Format |
| `regia editor` | Run the editor on a project; `--headless` runs it without a window for agents; see [Headless editor](../agents/headless-editor.md) |
| `regia viewer` | Product viewer packages: `viewer pack <kit> <recipe> -o <dir>` (cook, size report), `viewer verify <dir>`, `viewer standin -o <dir> [--pack]` (generic stand-ins and the demo page); see [Product viewer packages](../agents/viewer-pack.md) |
| `regia gen` | Generators (`generators/<name>.toml`): `list`, `run`, and `verify` (exit 1 with a diff list when a byte differs); see [Generators](../agents/generators.md) |
| `regia editor` | Run the editor on a project; `--headless` runs it without a window for CI and agents |
| `regia describe` | Write the project map (`project-map.md`, `project-map.json`): kinds, prefabs, components, the systems that read and write them, the levels using them; `--live` asks the running app, `--json` prints it, `--out DIR` (spec 101) |
| `regia preflight` | Project-side checks before a cook: the entity-kind lint at the project's `[kinds] strictness` or `--strictness off\|warn\|deny`; `--json`; exits non-zero on an error finding |
| `regia export` | `export glb --level L \| --prefab P [--entities GUIDS] --out FILE.glb`: a level, a prefab or some entities as one `.glb` for a DCC tool; `--skins` and `--animations` add skins and clips, `--keep-axes` keeps Regia's axes ([export guide](../agents/export-glb.md)) |
| `regia media` | `media encode <frames dir \| file.y4m> [file.wav] -o out.webm`: AV1 and Opus in WebM with Regia's own encoder, checked by the cook's validator; `--quality`, `--bitrate-kbps`, `--speed`, `--keyframe-interval`, `--audio-kbps`, `--threads`, `--json`; see [Video encoding](../agents/video-encode.md) |
| `regia golden` | Golden proposals agents made: `list`, `accept <id>` (at a terminal, typing the golden's name), `reject <id>`; see [Golden images](../agents/goldens.md) |
| `regia hooks` | The git pre-commit hook that runs `regia assets validate --staged` (the staged text assets parse from the index; the registry validators run over the staged paths when a cook exists): `hooks install [--force]`, `hooks uninstall [--force]`, `hooks status`, all with `--project DIR` and `--json`; see [Project health and staged validation](../agents/project-health.md) |
| `regia terrain` | Mesh layers and passages: `passages import` (a DO27 `do27-tunnels/1` or `do27-tunnels-v2/1` description as `<map>.passages.ron`, `--meshlayers` with `--heightfield` adds the cooked ground to the summary), `passages export`, and `cook <map.meshlayers.ron>` (the cook report, `--json`); see [Mesh layers](../agents/terrain-mesh-layers-formats.md) |
| `regia logs` | `logs triage PATH...`: stitch split tester uploads (JSONL, extracted folders, ZIPs, numbered ZIP parts) and report per-session exit state, frame and network summaries, event rates, gaps and storms; `--out NEW_DIR`, `--json`; see [Log stitching and triage](../agents/logs-triage.md). `logs perf-gate --thresholds FILE LOG...`: judge those summaries (or `--run DIR` trace captures) against frame-time, hitch, RSS and VRAM thresholds; `--baseline`, `--regression-pct`, `--res`, `--tier`, `--hitch-ms`, `--json`; exit 0 pass, 1 fail, 2 bad input; see [Performance gate](../agents/perf-gate.md) |
| `regia contact-sheet` | A labeled JPEG grid and a luma CSV: `--frames`, `--dirs` or `--pairs BEFORE AFTER`, `--out`, `--cols`, `--max-width`, `--quality`, `--label`, `--roi`, `--no-metrics`. Exit 0 wrote the files. Exit 2 is `contact_sheet_usage`, `contact_sheet_io` or `contact_sheet_format`. See [Contact sheet, install tier and grounded placement](../agents/content-smalls.md) |
| `regia rules` | Rule sets (`.rules.ron`, a match mode as data): `rules check <file>...` parses, checks and resolves each file and prints errors with their codes and field paths |

## `regia-editor`

| Option | Meaning |
|---|---|
| (none) | Open the project browser |
| `--project DIR` | Open a project and its startup level |
| `--game CRATE` | Host this game crate's library for hot reload |
| `--exit-after-frames N` | Quit after N frames (automation) |
| `--headless`, `--no-gpu` | No window, as for `regia editor --headless` |
| `--exit-on-stdin-eof` | With `--headless`: end, saving, when stdin reaches its end (`regia mcp --headless` starts it this way) |

See [Editor](../user-guide/editor.md).

## `cargo xtask`

Repository automation, run from the engine checkout.

| Command | What it does |
|---|---|
| `check-layers` | Crate dependency layering and runtime exclusions (no editor or pipeline crates in runtime builds) |
| `check-versions` | One version of each coupled crate (wgpu/egui/winit, glam) in runtime crates |
| `notices [--check]` | Generate or verify `THIRD_PARTY_NOTICES.md` |
| `build-times [--no-clean]` | Measure build times against the budgets |
| `check-exports [--lib PATH]` | Dynamic-symbol budget of `regia_dylib` (warn at 50 000, fail at 60 000) |
| `android-lifecycle --package ID` | Drive an installed Android app through home/resume cycles and rotations; fail on crash or leak |
| `soak --exe EXE --minutes M [...]` | Long-run stability sessions checked against `budgets/*.ron` ([Profiling](../systems/profiling.md)) |
| `budgets check` | Re-check soak reports against a budgets file |
| `fetch-test-assets` | Download the Khronos glTF sample models into `.regia/test-assets/` |
| `book-check` | Verify this book: dead links, agent-method coverage, summary completeness, `ron,<format>` blocks parse, `rust` blocks compile. Also runs `agent-coverage` when the agent dispatcher is in the tree |
| `agent-coverage` | Every method the agent dispatcher registers has a schema entry, an `agent.guide` topic, a coverage-map row, a guide-page mention, and a test that calls it |
| `manual [--check] [--no-mdbook]` | Regenerate the reference pages from the code, write `docs/manual/manual.html`, and build the mdBook site when `mdbook` is installed |
| `check-formats [--update]` | Frozen content-format shapes against `docs/formats/format-hashes.ron` |
| `check-unwraps` | No unjustified `unwrap`/`expect`/`panic!` on the untrusted-input boundary |
| `check-shaders` | Create every engine shader under the Vulkan validation layer |
| `minidump FILE [--symbols DIR]` | The crash reason and crashing thread's stack from a native crash bundle's `minidump.dmp` (or a bare `.dmp`), frames resolved with `addr2line` against a release's `symbols/<binary>.debug` ([Feedback and bug reports](../user-guide/feedback.md)) |

## `regia play-gate`

`regia play-gate replay TRACK [--log FILE] [--json] [--require-clean-log]` replays a `.track.ron` key track and checks log lines. `regia play-gate check TRACK [--json]` parses the file. Exit 0 pass, 1 missed expect or ERROR log, 2 bad track. See [Play gate](../agents/play-gate.md).
