// SPDX-License-Identifier: MPL-2.0
//! Repository automation for Regia Engine. Run with `cargo xtask <command>`.
//!
//! - `check-packaging`: no crate reads a file outside its own folder at compile time (`cargo package`/`cargo vendor`, ADR-1485).
//! - `check-layers`: crate DAG and runtime exclusions (`docs/02-architecture.md` § 1, `docs/06` § 7).
//! - `check-server-build`: the dedicated-server build compiles (spec 76 § 2).
//! - `check-net-core`: the ECS-free core of `regia_net` builds, links no ECS crate and runs the
//!   two-client headless example without the `ecs` feature (spec 120 § 3.2 and § 15.5, ADR-8501).
//! - `check-features [--list] [--set NAME] [--packages a,b] [--check]`: compile and lint every optional
//!   Cargo feature set the default build never builds, one row at a time (ADR-2322, `docs/04` § 11).
//! - `check-versions`: one version per coupled crate in runtime crates (`docs/03` § 2).
//! - `duplicate-deps [--update]`: fail when `cargo tree -d --workspace -e normal` shows a crate
//!   resolved to more than one version that `docs/ci/duplicate-deps.txt` does not record with a
//!   reason (the duplicate ratchet, ADR-3301).
//! - `notices [--check] [--strict]`: generate or verify `THIRD_PARTY_NOTICES.md` (crates + `docs/ported-sources.toml`)
//!   and check that every tracked binary asset has a row in `docs/asset-licences.toml`.
//! - `spdx [--check | --write]`: the `// SPDX-License-Identifier:` first line of every first-party `.rs`/`.wgsl`
//!   file (MPL-2.0; MIT-0 in samples and templates; a ported file's upstream id), ADR-10225.
//! - `sbom [--profile engine|game] [--out FILE] [--timestamp]`: CycloneDX 1.5 bill of materials (ADR-2760).
//! - `demo-notices [--check] [--verify DIR]`: the free demo's notices from the `showcase` tree, plus the offline/privacy rules (ADR-3111).
//! - `build-times [--no-clean]`: measure the build budgets of `docs/03` § 6.
//! - `check-exports`: dynamic-symbol budget for `regia_dylib` (ADR-015, `docs/06` § 5).
//! - `android-pass --package <id>`: install, lifecycle, presets, DO27 map and thermal run on a device.
//! - `android-budgets --run <dir> [--budgets <file>]`: one verdict per budget for a finished `android-pass` run.
//! - `android-lifecycle --package <id>`: home/resume, rotation and leak check on a device (`specs/11` § 8).
//! - `soak`: long-run stability sessions with budgets (`docs/06` § 4, M18).
//! - `book-check`: verify the user and agent guide, then provenance records — links, coverage and source declarations.
//!   Also runs `agent-coverage` when the agent dispatcher is in the tree.
//! - `preflight [--changed] [--no-build]`: fmt, clippy, rustdoc, book, shaders, unwraps, layers, versions,
//!   duplicate deps, notices and the test build in gate order, stopping at the first failure (`docs/04-conventions.md` § 11).
//! - `shuffle-check [--seeds N]`: the smoke scene under N seeded system orders; diverging seeds and the
//!   schedule ambiguity report.
//! - `repro-check [--seeds N] [--steps N]`: record/replay determinism stress (3 scenes × N seeds);
//!   diverging scene, seed, step and components.
//! - `editor-monkey [--seeds N] [--steps M] [--seed S] [--plant panic@K|nan@K]`: the editor monkey test in release;
//!   `--seeds 50 --steps 5000` is the nightly soak (ADR-3290).
//! - `adr-index [--check]`: generate `docs/decisions-index.md` from `docs/decisions.md` and report ADR consistency problems.
//! - `llms-txt [--check] [--out DIR]`: `llms.txt` and `llms-full.txt` for AI assistants, into `dist/public-root/` (ADR-10135).
//! - `onboarding-lint`: the user onboarding files agree with the onboarding contract and the `regia` CLI (ADR-10135, ADR-10230).
//! - `release-notes --from REV --to REV [--tag TAG] [--out FILE]`, `--check`, `--archive`: one note from the fragments a range adds; lint fragments (ADR-2767).
//! - `agent-coverage`: every method `regia_agent` dispatches has a schema entry, an `agent.guide` topic,
//!   a `book/COVERAGE.md` row, a guide-page mention and a test that calls it.
//! - `editor-agent-coverage [--update]`: the same rules for every editor agent method, read from
//!   `docs/api-snapshots/editor-agent-methods.txt` (ADR-7307); `book-check` runs it too.
//! - `book-coverage [--json]`: host extension methods, Cargo features and `regia` subcommands each have a
//!   `book/COVERAGE.md` row and a guide page (spec 96); `book-check` runs it too.
//! - `agent-examples [--print-baseline]`: the JSON examples in the agent guide against the protocol (a ratchet over
//!   `docs/ci/agent-examples.toml`).
//! - `alpha-triage`: turn DO27 alpha session logs (Regia Engine telemetry JSONL and feedback bundles)
//!   into a triage report (markdown + JSON summary).
//! - `manual [--check] [--no-mdbook]`: regenerate the reference pages, write `docs/manual/manual.html`, build the mdBook site.
//! - `web-player [--out DIR] [--no-opt] [--json]`: the web player's static files and their size report.
//! - `check-formats [--update] [--since-tag <tag-or-file>]`: frozen content-format serde shapes vs
//!   `docs/formats/format-hashes.ron`, and at most one version step per format since the last release (ADR-7724).
//! - `perf-smoke [--update]`: five CPU microbenchmarks vs `docs/perf/smoke-baseline.json` (3x tolerance).
//! - `perf-budgets [--filter] [--enforce] [--baseline] [--update-baseline] [--counts-only] [--json]`: spec CPU budgets
//!   in `docs/perf/budgets.ron` against their release measurements and a per-host baseline (ADR-2704).
//! - `check-unwraps`: the untrusted-input boundary must carry no unjustified `unwrap`/`expect`/`panic!`.
//! - `det-scan [--json]`: the lockstep determinism scan over `docs/ci/lockstep-closure.toml`: std
//!   transcendentals, direct libm, fast-math, f64 parse, hash containers, rayon and SIMD types in lockstep
//!   code, the closure's workspace dependencies and each crate's lockstep `clippy.toml` (ADR-8013).
//!   `--external ROOT --crate NAME` scans selected game packages; `--math-module PATH` names their
//!   libm boundary and `--with-closure` opts into their closure policy (ADR-8192).
//!   `--list-rules` generates the rules-and-rationale catalog.
//! - `check-shaders`: create every engine shader module (render pipelines, plus terrain, UI, VFX and lookdev WGSL) under Vulkan validation, then run the DX12 and MSL checks (`xtask::dx12_shaders`) over the other crates' shaders; `--matrix [--json]` runs the shader permutation matrix (ADR-2644).
//! - `golden-sheets --from <rev> --to <rev>`: contact sheets of every golden PNG that changed between two revisions.
//! - `smoke`: engine side of the DO27 alpha smoke list (PASS / FAIL / N-A, JSON and Markdown).
//! - `editor-smoke [--only GLOB] [--skip GLOB] [--card N] [--windowed] [--keep-going]`: drive every editor panel,
//!   menu item, command and action through the agent protocol and report what broke (ADR-7865).
//!   `--scenes` runs only rows 19–21 (ultrawide, black sky, many units).
//! - `bake-check [--gpu]`: baked light end to end on the fixture project (export, bake, level,
//!   check, sample, runtime install; ADR-365).
//! - `replay-soak`: record and replay N seeds of the replay-soak scenes per compute-thread count and
//!   build; state-hash traces must match across runs and every replay its recording (ADR-867).
//! - `gpu-bench --suite bindless|gi|baked-light --out DIR`: bench suite on the card `REGIA_GPU_INDEX` selects (JSON + markdown).
//! - `showcase-table [--scene DIR] [--tiers LIST] [--size WxH] [--cpu] [--two-phase on|off]`: the showcase performance table (per-view min/avg/max fps and p99
//!   per quality tier on one pinned card; `docs/perf/showcase-table.md`, ADR-2444).
//! - `player-sweep`: cold first launch, replay (default 10 minutes), quits and a crash test.
//!   Each child writes spec 28 `summary.json`. The table is
//!   `.claude/orchestration/reports/sweep-<date>.md`.
//! - `gi-compare --scene <fixture> [--views <views.ron>] [--spp N]`: baked light against the path tracer,
//!   per view, with contact sheets and `summary.json`; `--read <summary.json>` prints the table.
//! - `dist --target windows|deck [--dry-run] [--from DIR] [--git REV] [--readme FILE] [--verify PATH]`: a test bundle (binaries,
//!   licenses, README, pipeline caches, `dist-manifest.json`) as a zip (Windows) or tar (Steam Deck).
//! - `demo-package [--scene DIR] [--targets windows,linux] [--app-id N] [--check] [--verify DIR]`: the free-camera demo as bundles, a
//!   Steam depot folder, archives with checksums and the Deck check steps; fails on DO27 content or an unlicensed asset (ADR-2827).
//! - `flaky [--check] [--gate-args lvp|gpu]`: the flaky-test registry (`docs/ci/flaky-tests.toml`).
//! - `check-ecology`: every `.biome.ron` validated for plausible ecology with its `.ecology.ron` (ADR-2456).
//! - `binary-size [--target T] [--template ID] [--json] [--check] [--update]`: shipping binary size by crate against `docs/ci/size-budgets.toml` (ADR-3188).
//! - `host-strings [--binary PATH]... [--allow FILE]`: scan built executables' printable strings for private build-machine names and paths, the same gate `regia package` enforces; default target is the release `hello_window` sample, built on demand (ADR-10354).
//! - `templates-check [--template ID] [--no-run]`: `regia new` for every template, build the project, run its game for 5 frames (ADR-2046).
//! - `two-tag-upgrade --from TAG --to TAG [--templates all|a,b]`: `regia new` with the old tag's CLI, `upgrade-project` with the new one, `cargo check` (spec 94 § 9, ADR-2610).
//! - `nightly [--check]`: the nightly and periodic job registry (`docs/ci/nightly-jobs.toml`).
//! - `walkthrough [--stage S] [--template ID] [--keep] [--list]`: the book's getting-started path as a stranger (ADR-2954).
//! - `api-snapshot [--update [--breaking "<why>"]]`: the stable crates' public API against `docs/api-snapshots/`; fails on a removed or changed item.
//! - `check-provenance`: source classes, public-page URLs and reader/builder records (ADR-4581).
//! - `test-costs [--packages a,b] [--out DIR]`: disk, link time, test count and run time of every test binary (ADR-2986).
//! - `flash-check <dir> --fps N [--json]`: photosensitivity check of a PNG sequence (WCAG 2.2 SC 2.3.1, ADR-3804).
//! - `agent-snapshot [--update|--check]`: the agent protocol surface against
//!   `docs/api-snapshots/agent-protocol.txt`; a breaking change needs the method named under
//!   `## Upgrade` in a touched release fragment, an additive one a `## New` line, and `--update`
//!   refuses to write an undocumented change (ADR-3335).
//! - `release-ledger [--no-scratch] [--no-ancestry] [--print-open] [--strict] [--json]`: the v0.5.0 evidence ledger's rows, commits,
//!   evidence files and integration claims (ADR-4935).
//! - `loc-length --ftl FILE --budget FILE`: pseudo-localization length budgets. Each Fluent message's
//!   pseudo-localized length is compared with `key = max_chars`. Exit 0 when every budgeted key fits;
//!   exit 1 prints each key that exceeds, with its pseudo length and budget, sorted by key (ADR-4564).
//! - `package-smoke [--seconds N] [--android] [--android-emulator] [--dry-run] [--windows]`: package dev and
//!   shipping builds, play a scripted input recording, and fail on an `ERROR` log record or a dirty
//!   `summary.json` (ADR-3968).
//! - `public-export --rev REV --out DIR [--check DIR] [--manifest FILE] [--hash-term TERM]`: the public
//!   repository's single-commit snapshot of one revision and its leak check (ADR-10229).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};

mod fuzz;

use xtask::book_coverage;
use xtask::demo_legal;
use xtask::editor_monkey;
use xtask::graph::Graph;
use xtask::net_match;
use xtask::net_soak;
use xtask::rename_brand;
use xtask::{
    adr_index, agent_bench, agent_coverage, agent_examples, agent_snapshot, alpha_triage, android_lifecycle,
    android_pass, api_snapshot, bake_check, binary_size, book, build_times, check_ecology, check_formats,
    check_unwraps, ci_registry, coverage, duplicate_deps, exports, feature_matrix, fetch_test_assets,
    flash_check, gi_compare, golden_sheets, gpu_bench, host_strings, layers, manual, mutants, notices,
    perf_budgets, perf_smoke, plate_floors, player_sweep, preflight, release_notes, replay_soak, repro_check,
    sbom, showcase_table, shuffle_check, smoke, soak, spdx, stream_bench, sweep, templates_check, two_tag,
    walkthrough, web_player, windows_pass,
};

#[derive(Parser)]
#[command(about = "Regia Engine repository automation")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check the crate layering rules.
    CheckLayers,
    /// The kind lint over every sample project at its own `[kinds] strictness` (spec 101 § 5).
    CheckKinds,
    /// Compile the dedicated-server build (spec 76 § 2): `check-layers` reads its graph, this proves
    /// no simulation module reaches a client module.
    CheckServerBuild,
    /// The net-core gate check (spec 120 § 15.5, ADR-8501): the no-ECS dependency assertion over
    /// `cargo tree -p regia_net --no-default-features`, `cargo check -p regia_net --no-default-features`
    /// with the `net_core_headless` example, and that example's two-client loopback test without
    /// the `ecs` feature.
    CheckNetCore,
    /// Compile and lint every optional Cargo feature set the default workspace build never builds
    /// (the feature matrix, ADR-2322); lists every failing row.
    CheckFeatures(feature_matrix::Args),
    /// Compare the stable crates' public API with `docs/api-snapshots/`; fail on a removed or changed item.
    ApiSnapshot(api_snapshot::Args),
    /// Compare the agent protocol surface with `docs/api-snapshots/agent-protocol.txt`; fail on a
    /// breaking change without a release-fragment `## Upgrade` line or on any undocumented change.
    AgentSnapshot(agent_snapshot::Args),
    /// Count the compiled and ignored doc examples of the stable crates against `docs/ci/doc-examples.toml`.
    DocExamples(xtask::doc_examples::Args),
    /// Check that coupled crates resolve to one version in runtime crates.
    CheckVersions,
    /// Fail when `cargo tree -d` shows a duplicate crate that `docs/ci/duplicate-deps.txt`
    /// does not record with a reason (the duplicate ratchet, ADR-3301).
    DuplicateDeps {
        /// Rewrite the baseline from the current tree (new rows get an empty reason).
        #[arg(long)]
        update: bool,
    },
    /// Generate THIRD_PARTY_NOTICES.md, or with --check fail if it is out of date.
    Notices {
        /// Fail instead of writing when the file is out of date.
        #[arg(long)]
        check: bool,
        /// Also fail on binary assets whose licence is `unknown` in `docs/asset-licences.toml`.
        #[arg(long)]
        strict: bool,
    },
    /// Check (default) or write the `// SPDX-License-Identifier:` first line of every first-party
    /// `.rs` and `.wgsl` file (ADR-10225).
    Spdx {
        /// Fail when a file lacks its line (the default).
        #[arg(long, conflicts_with = "write")]
        check: bool,
        /// Insert or correct the line in every file that needs it.
        #[arg(long)]
        write: bool,
    },
    /// Write a CycloneDX 1.5 JSON bill of materials: crates, ported sources and the asset inventory.
    Sbom {
        /// `engine`: everything Regia Engine ships; `game`: what a shipped game can link.
        #[arg(long, value_enum, default_value = "engine")]
        profile: sbom::Profile,
        /// Write to this file instead of stdout.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Add a `metadata.timestamp` (off by default so the output is reproducible).
        #[arg(long)]
        timestamp: bool,
        /// `game` profile: `crate/feature` names the game turns on, on top of each runtime crate's
        /// defaults and the umbrella's media codecs (comma-separated or repeated), e.g.
        /// `regia_steam/steamworks,regia_net/net-quic`.
        #[arg(long, value_delimiter = ',')]
        features: Vec<String>,
        /// `game` profile: resolve every optional feature of every runtime crate (the notices set).
        #[arg(long)]
        all_features: bool,
    },
    /// Generate the free demo's notices (`docs/demo/THIRD_PARTY_NOTICES-demo.md`) from the demo
    /// packaging tree of `showcase`, and fail when that tree links anything that could upload data
    /// (ADR-3111, ADR-4847).
    DemoNotices {
        /// Fail instead of writing when the file is out of date.
        #[arg(long)]
        check: bool,
        /// Instead of writing, verify a packaged demo folder: EULA approved and present, notices
        /// present and current, no uploader or network stack in its binaries. Writes nothing.
        #[arg(long, value_name = "DIR")]
        verify: Option<std::path::PathBuf>,
    },
    /// Measure build times against the budgets.
    BuildTimes {
        /// Reuse the previous build-times target directory instead of starting clean.
        #[arg(long)]
        no_clean: bool,
    },
    /// Count `regia_dylib` dynamic symbols. Warn at 50,000, fail at 60,000.
    CheckExports {
        /// Library to count. When omitted, this builds `-p regia_dylib` first.
        #[arg(long)]
        lib: Option<PathBuf>,
    },
    /// Drive an installed Android app through home/resume cycles and rotations; fail on crash or leak.
    AndroidLifecycle(android_lifecycle::Args),
    /// The owner's one-command real-device pass: install, lifecycle, presets, DO27 map, thermal run.
    AndroidPass(android_pass::Args),
    /// The verdict per budget of a finished `android-pass` run.
    AndroidBudgets(android_pass::budgets::Args),
    /// The owner's one-command Windows pass: toolchain, test subset, Vulkan and DX12 runs, quits, pipeline-compile times; `--read` prints a verdict.
    WindowsPass(windows_pass::Args),
    /// Soak a built app for minutes per session and check the report against budgets (`docs/06` § 4).
    Soak(soak::SoakArgs),
    /// Budget helpers: check a soak report against `budgets/default.ron`.
    Budgets {
        #[command(subcommand)]
        command: soak::BudgetsCommand,
    },
    /// Download Khronos glTF sample models into `.regia/test-assets/` (gitignored).
    FetchTestAssets,
    /// Build the web player's static files and report their size (ADR-2541).
    WebPlayer(web_player::Args),
    /// Per-vendor PSNR floors of the golden plates: check a run, explain its failures with a lavapipe run, or
    /// measure a vendor (ADR-2089, ADR-5798).
    PlateFloors(plate_floors::Args),
    /// Contact sheets of every golden PNG that changed between two revisions.
    GoldenSheets(golden_sheets::Args),
    /// The Regia Editor's app icons (PNGs, .ico, .icns) from `crates/regia_editor/assets/icon/regia-editor.svg`
    /// (ADR-20000); `--check` compares instead of writing.
    EditorIcons(xtask::editor_icons::Args),
    /// DO27's frame-time bench on its real maps: builds `lookdev`'s `do27_bench` in release and
    /// runs it with these arguments (needs `REGIA_DO27_CONTENT`; see `samples/lookdev/src/bin/do27_bench.rs`).
    BenchDo27 {
        /// Arguments for `do27_bench` (`--maps`, `--tiers`, `--sizes`, `--out`, `--baseline`, ...).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Verify the book: dead links, schema method coverage and summary completeness.
    ///
    /// Also runs `agent-coverage` when `crates/regia_agent/src/dispatch.rs` exists.
    BookCheck {
        /// Only the fast pre-check: bare `rust` fences that cannot be a module body (no compile).
        #[arg(long)]
        fences: bool,
    },
    /// Host extension methods, Cargo features and `regia` subcommands each have a
    /// `book/COVERAGE.md` row and a guide page (spec 96). `book-check` runs the same checks.
    BookCoverage {
        /// Print the report as JSON (`format_version` 1) instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Baked light end to end on the fixture project: export, bake (CPU, or `--gpu`), level
    /// rewrite, `--check`, `--sample`, runtime install and lookup (ADR-365).
    BakeCheck(bake_check::Args),
    /// The merge gate's cheap checks in gate order (fmt, clippy, rustdoc, book, shaders, unwraps,
    /// layers, versions, notices, test build); stops at the first failure. Never edits files.
    ///
    /// On the workstation run it through the build wrapper (it prints the exact command).
    Preflight(preflight::Args),
    /// Run the headless smoke scene under N seeded system orders and report any run whose state
    /// differs, plus the engine schedules' ambiguity report (`book/agent-guide/schedule-shuffle.md`).
    ShuffleCheck(shuffle_check::Args),
    /// Record every determinism scene under N seeds, replay each recording and report any
    /// checkpoint whose state hash differs (`book/agents/determinism.md`).
    ReproCheck(repro_check::Args),
    /// The editor monkey test (`crates/regia_editor/tests/monkey.rs`, ADR-3290) in a release build at a chosen size:
    /// `--seeds 50 --steps 5000` is the nightly soak; `--plant panic@STEP` proves the driver catches a fault.
    EditorMonkey(editor_monkey::Args),
    /// Record and replay N seeds of every replay-soak scene once per compute-thread count and build
    /// profile; fail when a record trace differs between runs or a replay leaves its recording
    /// (`book/agents/replay-soak.md`). Writes `replay-soak.md` and `replay-soak.json` in `--out`.
    ReplaySoak(replay_soak::Args),
    /// Generate `docs/decisions-index.md` (one row per ADR, relations, unused numbers, consistency checks).
    AdrIndex(adr_index::Args),
    /// Build one release note from the fragments a revision range adds (`--from`/`--to`), lint fragments
    /// (`--check`) or move used fragments to `docs/releases/fragments/<tag>/` (`--archive`).
    ReleaseNotes(release_notes::Args),
    /// Every dispatched agent method has a schema, a guide topic, a coverage row and a test call.
    AgentCoverage,
    /// Every editor agent method (`docs/api-snapshots/editor-agent-methods.txt`) has a coverage row,
    /// a page naming it and each param, a guide topic and a test call (ADR-7307).
    EditorAgentCoverage {
        /// Refresh the snapshot from the editor first (runs the `editor_method_snapshot` test).
        #[arg(long)]
        update: bool,
    },
    /// The agent benchmark (`bench/agent_tasks`, spec 23 § 8): play the oracles, serve a task to an
    /// outside MCP client, or check an engine a client has driven (`book/agents/agent-benchmark.md`).
    AgentBench(agent_bench::Args),
    /// The JSON examples in the agent guide against the protocol (`docs/ci/agent-examples.toml` is the baseline).
    AgentExamples {
        /// Print baseline rows for every current finding (reasons left as TODO) instead of checking.
        #[arg(long)]
        print_baseline: bool,
    },
    /// Build the Regia Engine Manual: regenerate `book/reference/` pages from the code, write the
    /// single-file `docs/manual/manual.html`, and run `mdbook build book` when mdbook is installed.
    Manual {
        /// Write nothing; fail when a generated page or `manual.html` is out of date.
        #[arg(long)]
        check: bool,
        /// Skip `mdbook build` even when `mdbook` is on `PATH`.
        #[arg(long)]
        no_mdbook: bool,
    },
    /// Every `.biome.ron` in the repository validated for plausible ecology with the `.ecology.ron`
    /// beside it (ADR-2456): fails on a species mix that could not grow together, with the reason.
    CheckEcology,
    /// Check that frozen content formats' serde shapes match the committed hashes (specs/12 § 4a).
    CheckFormats {
        /// Re-record the schema hashes after a deliberate version bump + migrator.
        #[arg(long)]
        update: bool,
        /// The last release's format table (a file, or a git tag whose `docs/formats/format-hashes.ron` is read)
        /// for the one-step-per-release rule (ADR-7724); default `docs/formats/format-hashes-v0.6.0.ron` (ADR-10183).
        #[arg(long, value_name = "TAG_OR_FILE")]
        since_tag: Option<String>,
    },
    /// Five CPU microbenchmarks against `docs/perf/smoke-baseline.json` with a generous tolerance (3x). Release only;
    /// `--update` re-records the baseline on the reference machine.
    PerfSmoke(perf_smoke::Args),
    /// Spec CPU budgets in `docs/perf/budgets.ron` against their release measurements and a per-host baseline, with
    /// machine-independent allocation counts (`--counts-only`). Exit code non-zero only with `--enforce`.
    PerfBudgets(perf_budgets::Args),
    /// Generate every project template with `regia new`, build it and run its game for five frames
    /// (the `templates` CI job, `docs/06` § 5). `--no-run` builds only.
    TemplatesCheck(templates_check::Args),
    /// Size of the shipping binary of the given templates by crate, symbol and generic instantiation; `--check` compares
    /// with `docs/ci/size-budgets.toml`, `--update` re-records (ADR-3188).
    BinarySize(binary_size::Args),
    /// Scan built executables' printable strings for private build-machine names and paths
    /// (ADR-10354); default target is the release `hello_window` sample, built on demand.
    HostStrings(host_strings::Args),
    /// Check every provenance row for an allowed source class and required public-page citations.
    CheckProvenance,
    /// Check the untrusted-input boundary for unjustified panic constructs (M18 robustness).
    CheckUnwraps,
    /// Scan Regia's lockstep closure or selected external workspace crates for code that can
    /// compute different bits on another host (ADR-8012 A-4, ADR-8013, ADR-8192).
    DetScan(xtask::det_scan::Args),
    /// Fail if a crate's compiled code reads a file outside its own folder, so `cargo package` and
    /// `cargo vendor` copies build (ADR-1485).
    CheckPackaging,
    /// Rename the engine to Regia Engine across a tree: crates, identifiers, env vars, docs, paths
    /// (ADR-1388, `docs/rename-regia.md`). Deterministic and idempotent; `--dry-run` writes nothing.
    RenameBrand(rename_brand::Args),
    /// Check the v0.5.0 release evidence ledger: every burn-down row, cited commit, evidence file
    /// and integration claim (`docs/release/v0.5.0-evidence-ledger.md`, ADR-4935).
    ReleaseLedger(xtask::release_ledger::Args),
    /// Engine side of the DO27 alpha smoke list. Prints PASS / FAIL / N-A / REPORT and writes
    /// `alpha-smoke.json` and `alpha-smoke.md`. Exit 1 when any row fails. A REPORT row
    /// is measured and printed and does not fail the run.
    ///
    /// Hitches are `do27_chase --soak minute` on both DO27 maps (judge a release build:
    /// debug frame times are not the alpha gate). The tunnel-interior plate writes the
    /// mouth screenshot. The whole run is meant to stay under 15 minutes on the 5080.
    Smoke(smoke::SmokeArgs),
    /// Launch the real editor on a copy of a sample project and drive every panel, menu item,
    /// palette command and registered action through the agent protocol (ADR-7865). Writes
    /// `target/editor-smoke/<timestamp>/` (result.json, contact sheets, summary.md); exit 1 on a
    /// FAIL. `book/agents/editor-smoke.md`.
    EditorSmoke(xtask::editor_smoke::EditorSmokeArgs),
    /// World streaming teleport bench (spec 78 acceptance B) on the selected card.
    StreamBench(stream_bench::Args),
    /// Measured code coverage of named crates through `cargo llvm-cov` (ADR-2705, `docs/06` § 15).
    Coverage(coverage::Args),
    /// Mutation testing of named crates through `cargo mutants`, with a checked-in skip list (ADR-3389, `docs/06` § 20).
    Mutants(mutants::Args),
    /// Nightly DO27 player sweep: cold launch, replay, quits, crash. Posts a markdown table.
    PlayerSweep(player_sweep::SweepArgs),
    /// Player sweep scenes at the standard profiles; writes `sweep.csv` and `sweep.json` under `--out`.
    ///
    /// Runs on the card the caller selected (`.claude/orchestration/nightly-sweep.sh` locks one).
    NightlySweep(sweep::NightlyArgs),
    /// Baked light against the path tracer: per view, the baked frame, live GI and the reference,
    /// full and indirect, on one contact sheet, plus `summary.json`. `--read` prints its table.
    GiCompare(gi_compare::Args),
    /// Bench suite (`bindless`, `gi`, `baked-light`) on the card `REGIA_GPU_INDEX` selects, using a
    /// prebuilt `do27_chase`; writes `gpu-bench.json` and `gpu-bench.md` under `--out`.
    ///
    /// Builds nothing, holds `/tmp/regia-gpu-<n>.lock` then `/tmp/ferrite-gpu-<n>.lock` per run and refuses GPU 6. The baked-light
    /// suite reports `not available` on a build without the baked-GI runtime.
    GpuBench(gpu_bench::BenchArgs),
    /// The showcase performance table: runs the prebuilt `showcase` host once per quality tier on the
    /// card `REGIA_GPU_INDEX` selects (or `--cpu` on lavapipe) and plays each camera view of the scene's
    /// `perf_views.json` at 1440p, then writes per-view min / average / max fps and p99 frame time to
    /// `showcase-table.md` and `.json` under `--out` (ADR-2444). Unmeasured rows say `not run`.
    ShowcaseTable(showcase_table::TableArgs),
    /// Create every engine shader under the Vulkan validation layer. Fails on any validation error.
    ///
    /// `regia_render` also builds its lab and Desktop High pipelines. Terrain, UI,
    /// VFX and the lookdev portal shader are parsed, `spirv-val`'d and created as modules.
    /// Then the DX12 checks (naga HLSL for Shader Model 5.1 and 6.0, Direct3D 12 compute limits,
    /// FXC's loop rule) run over those crates' shaders, the 2D sprite and the path tracer, and
    /// naga's MSL writer with wgpu's Metal options (ADR-2644).
    ///
    /// `--matrix` then runs the shader permutation matrix: every permutation of every shipped
    /// preset and platform (forward axes, lite tiers, material graph passes), deduplicated by
    /// composed-module hash, through naga, the layout scan, `spirv-val`, a pipeline per entry
    /// point on a validation-enabled device, HLSL (SM 6.0 and 5.1 with the FXC rules) and MSL,
    /// and renders each preset's frame. `--json` prints its per-module report on stdout.
    CheckShaders(CheckShadersArgs),
    /// Print the crash reason and the crashing thread's stack from a native crash bundle's
    /// minidump (or a bare `.dmp`). Resolves frames with `addr2line` against the release's split
    /// debug symbols (`regia package` writes `symbols/<binary>.debug`).
    Minidump {
        /// A `feedback-…-crash.zip` bundle or a `.dmp` file.
        path: PathBuf,
        /// Directory holding `<binary>.debug` files (a package's `symbols/`).
        #[arg(long)]
        symbols: Option<PathBuf>,
    },
    /// Turn DO27 alpha session logs (Regia Engine telemetry JSONL and feedback bundles) into a
    /// triage report: per-session frame-time tables, cross-session groups by GPU and preset,
    /// crash signatures and suspected engine issues. Markdown to stdout (or `--out`), JSON
    /// summary with `--json`.
    AlphaTriage(alpha_triage::Args),
    /// Build a Windows or Steam Deck test bundle: the binaries (default `vfx_lab`), the licenses,
    /// a README with the run steps and the log, crash-dump and pipeline-cache folders, and
    /// `dist-manifest.json` (every file's size and BLAKE3), as a zip (Windows) or tar (Deck).
    /// `--dry-run` prints the plan; `--verify PATH` checks a bundle or its archive.
    Dist(xtask::dist::Args),
    /// Build the free-camera demo for Linux and Windows (shipping profile, panic = abort) and lay it out
    /// for upload: the bundles, a Steam depot folder with steamcmd scripts, plain archives with
    /// `CHECKSUMS.b3` and the Steam Deck check steps. Fails on any Direct Order 27 content path and on an
    /// asset with no licence row. Uploads nothing and reads no credential.
    DemoPackage(xtask::demo_package::Args),
    /// Stage a finished demo package (`cargo xtask demo-package`) into the steamcmd content layout:
    /// Windows x64 and Linux x64 depots re-hashed against their manifests, app and depot build
    /// scripts, the store page text and the exact steamcmd commands. `--dry-run` is required: it
    /// never runs steamcmd, logs in or stores a credential (ADR-4899).
    SteamDemoUpload(xtask::steam_demo_upload::Args),
    /// The flaky-test registry (`docs/ci/flaky-tests.toml`): print and check it, or print the
    /// gate test runner's `--solo` / `--quarantine` arguments for one pass.
    ///
    /// The check fails when an entry names a package, test target or test function that does
    /// not exist, or breaks the retry policy (`book/user-guide/flaky-tests.md`).
    Flaky(ci_registry::FlakyArgs),
    /// The nightly and periodic job registry (`docs/ci/nightly-jobs.toml`): print and check it.
    ///
    /// The check fails when a job runs an xtask sub-command, package, test target or test
    /// function that does not exist (a `planned` job may name a sub-command another branch adds).
    Nightly(ci_registry::NightlyArgs),
    /// The two-tag distribution upgrade check (spec 94 § 9): build the `regia` CLI of two tags in git
    /// worktrees, create every template with the old one, upgrade with the new one, `cargo check` the result.
    TwoTagUpgrade(two_tag::Args),
    /// The getting-started path run as a stranger would, from the book's `walkthrough=` fences:
    /// engine archive, install, doctor, every template new, smoke, cook, package, run the package,
    /// upgrade-project, the Android check (ADR-2954). Writes `target/walkthrough/report.json`.
    Walkthrough(walkthrough::Args),
    /// Coverage-guided fuzzing of the fuzz-target registry (nightly, cargo-fuzz; ADR-2723): `list`, `seed`, `build`,
    /// `run`, `run-all`, and `replay` of the committed findings on stable.
    Fuzz(fuzz::Args),
    /// Per test binary: disk, link time, build-unit time, test count and run time; per test: run time.
    /// Writes `test-costs.csv`, `slowest-tests.csv` and `test-costs.md` (ADR-2986, `docs/06` § 9).
    ///
    /// Builds in its own target directory (default `target/test-costs/target`): the link-timing shim
    /// changes cargo's fingerprint. Asserts no time; the report records the host load.
    TestCosts(xtask::test_costs::Args),
    /// Photosensitivity check of a directory of PNG frames (WCAG 2.2 SC 2.3.1, ADR-3804).
    /// Exit 0 passes, 2 fails the thresholds, 1 is a usage or I/O error.
    FlashCheck(flash_check::Args),
    /// The `net-game` template across real machines (ADR-3075): `generate` writes the project for a
    /// build host; `run` plays a dedicated server, windowed and headless clients over UDP with
    /// stated conditions and writes the report (match length, reconciliation, bandwidth, desync).
    NetMatch(net_match::Args),
    /// `regia_net`'s server and clients under bad networks (ADR-7899): impairment profiles, a sample
    /// game at N clients, bars; writes `result.json` and `summary.md` and exits 1 when a bar fails.
    NetSoak(net_soak::Args),
    /// Scan Android cfg usage in crate sources and check the CLI and templates for Android aarch64.
    AndroidCfg,
    /// Fail when a Fluent message's pseudo-localized length exceeds its character budget (ADR-4564).
    ///
    /// `--ftl` is the `.ftl` file and `--budget` is `key = max_chars` lines. The defaults match
    /// `PseudoOptions`: expansion 0.3, accents on, brackets on. `--no-accents` and `--no-brackets`
    /// turn those off. A message with no budget is skipped and listed. A budget key missing from
    /// the file is `loc_budget_missing`. Exit 0 when every budgeted key fits. Exit 1 when one or
    /// more exceed (or a budget key is missing, or a budget line does not parse), printing
    /// `key`, the pseudo-localized length and the budget, one per line, sorted by key.
    LocLength(xtask::loc_length::Args),
    /// Check local links in docs/ against docs/ci/docs-links.toml.
    DocsLinks,
    /// Write `llms.txt` and `llms-full.txt` (the book's index and the user-facing book in one file, for
    /// AI assistants) into `dist/public-root/`; `--check` fails when either is stale, a linked page is
    /// missing, or a page has no title or description (ADR-10135).
    LlmsTxt(xtask::llms_txt::Args),
    /// Check the user onboarding files (`dist/public-root/`, `dist/public-skills/`, the assistant book
    /// pages) against the onboarding contract and the `regia` CLI: question ids, commands, skills,
    /// forbidden terms (ADR-10135, ADR-10230).
    OnboardingLint,
    /// Package dev and shipping builds, play a scripted input recording for `--seconds` (default
    /// 60), and fail on an `ERROR` JSON-log record or a dirty `summary.json` (ADR-3968).
    ///
    /// `--android` also builds the sim_bridge APK and AAB and records sizes (shipping APK limit
    /// 150 MiB). `--android-emulator` boots an x86_64 API 35 image when `/dev/kvm` is usable.
    /// `--dry-run` prints those emulator commands and does not boot. `--windows` on Linux prints
    /// the Windows a test machine's commands and does not build. `--write-recordings` rewrites the
    /// committed `.frec` files.
    PackageSmoke(xtask::package_smoke::Args),
    /// Run the determinism goldens on several targets and compare them (ADR-8081): `stage`, `run`,
    /// `gate` (the host and qemu-aarch64 legs) and `verify <dir>`.
    ///
    /// Legs: host, qemu (a static aarch64 binary under qemu-aarch64 user mode), android (`--adb
    /// <serial>`), mac, windows, deck. A run fails unless the clean scenarios hash equal on every
    /// leg and the planted glam SIMD scenario differs on every aarch64 leg.
    DetGolden(xtask::det_golden::Args),
    /// Export one revision as the public repository's single-commit snapshot (`--rev REV --out DIR`) and
    /// scan an export for private hosts, paths, names, secrets, large files and missing licence files
    /// (`--check DIR`), under the reviewed manifest `xtask/public-export.toml` (ADR-10229).
    PublicExport(xtask::public_export::Args),
}

/// Every sub-command name and alias of this binary, for `nightly --check`.
fn subcommand_names() -> Vec<String> {
    Cli::command()
        .get_subcommands()
        .flat_map(|c| std::iter::once(c.get_name()).chain(c.get_all_aliases()))
        .map(str::to_string)
        .collect()
}

fn workspace_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(PathBuf::from)
        .context("xtask has no parent directory")
}

fn report(name: &str, errors: Vec<String>) -> ExitCode {
    if errors.is_empty() {
        println!("{name}: ok");
        ExitCode::SUCCESS
    } else {
        for e in &errors {
            eprintln!("error: {e}");
        }
        eprintln!("{name}: {} problem(s)", errors.len());
        ExitCode::FAILURE
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let root = workspace_root()?;
    Ok(match cli.command {
        Cmd::CheckKinds => report("check-kinds", xtask::check_kinds::check(&root)?),
        Cmd::CheckLayers => {
            let declared = Graph::declared()?;
            let mut errors = layers::check_direct(&declared);
            // specs/27 § 1.1: the reference path tracer never reaches a runtime crate, by any feature.
            errors.extend(layers::check_offline_only(&declared));
            for name in layers::runtime_members(&declared) {
                errors.extend(layers::check_runtime_tree(&Graph::tree(&root, &name)?));
            }
            errors.extend(layers::check_server_tree(&Graph::server_tree(&root)?));
            // Spec 120 § 15.5: the regia_net core links no ECS crate.
            let (net, net_args) = layers::NET_CORE_TREE;
            errors.extend(layers::check_net_core_tree(&Graph::tree_with_args(
                &root, net, net_args,
            )?));
            // ADR-7972: the player-build pipeline (`default-features = false`) links no TLS stack and
            // no optional importer. Resolved for the crate alone, so the workspace's defaults do not unify in.
            errors.extend(layers::check_pipeline_runtime_tree(&Graph::tree_with_args(
                &root,
                "regia_asset_pipeline",
                &["--no-default-features"],
            )?));
            report("check-layers", errors)
        }
        Cmd::CheckServerBuild => {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo)
                .current_dir(&root)
                .args(["check", "--locked", "-p", "dedicated_server"])
                .args(["--no-default-features", "--features", "server"])
                .status()?;
            let mut errors = Vec::new();
            if !status.success() {
                errors.push("the server build does not compile (spec 76 § 2)".to_string());
            }
            report("check-server-build", errors)
        }
        Cmd::CheckNetCore => {
            let (net, net_args) = layers::NET_CORE_TREE;
            let mut errors = layers::check_net_core_tree(&Graph::tree_with_args(&root, net, net_args)?);
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            for (args, what) in xtask::net_core::COMMANDS {
                let status = std::process::Command::new(&cargo)
                    .current_dir(&root)
                    .args(*args)
                    .status()?;
                if !status.success() {
                    errors.push(format!("{what} failed: cargo {}", args.join(" ")));
                }
            }
            report("check-net-core", errors)
        }
        Cmd::CheckVersions => {
            let mut versions = BTreeMap::new();
            for name in layers::runtime_members(&Graph::declared()?) {
                layers::collect_coupled(&Graph::tree(&root, &name)?, &mut versions);
            }
            for (name, v) in &versions {
                println!(
                    "  {name}: {}",
                    v.iter().map(String::as_str).collect::<Vec<_>>().join(", ")
                );
            }
            report("check-versions", layers::check_versions(&versions))
        }
        Cmd::DuplicateDeps { update } => report("duplicate-deps", duplicate_deps::run(&root, update)?),
        Cmd::Sbom {
            profile,
            out,
            timestamp,
            features,
            all_features,
        } => {
            let selection = sbom::GameFeatures {
                all: all_features,
                features,
            };
            let text = sbom::run_with(&root, profile, timestamp, &selection)?;
            match out {
                Some(path) => {
                    std::fs::write(&path, &text).with_context(|| format!("writing {}", path.display()))?;
                    println!("sbom: wrote {} ({} profile)", path.display(), profile.name());
                }
                None => print!("{text}"),
            }
            ExitCode::SUCCESS
        }
        Cmd::Spdx { check: _, write } => {
            let ported = notices::load_ported(&root)?;
            let ids = spdx::ported_ids(&ported);
            let missing = spdx::missing_ported(&root, &ids);
            for path in &missing {
                eprintln!(
                    "spdx: {} lists `{path}`, which does not exist",
                    notices::PORTED_SOURCES
                );
            }
            let findings = spdx::run(&root, &ids, write)?;
            if write {
                println!("spdx: wrote the line into {} file(s)", findings.len());
                if missing.is_empty() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            } else if findings.is_empty() && missing.is_empty() {
                println!("spdx: ok");
                ExitCode::SUCCESS
            } else {
                for finding in findings.iter().take(200) {
                    eprintln!("spdx: {finding}");
                }
                if findings.len() > 200 {
                    eprintln!("spdx: ... and {} more", findings.len() - 200);
                }
                eprintln!(
                    "spdx: {} file(s) need their SPDX line; run `cargo xtask spdx --write`",
                    findings.len()
                );
                ExitCode::FAILURE
            }
        }
        Cmd::Notices { check, strict } => {
            let path = root.join("THIRD_PARTY_NOTICES.md");
            let shipped = notices::shipped_crates()?;
            let inputs = notices::resolve_inputs(&root, &shipped)?;
            let ported = notices::load_ported(&root)?;
            let mut problems = notices::validate_ported(&ported);
            problems.extend(notices::check_ported_files(&root, &ported));
            let do27 = xtask::do27_ports::load(&root)?;
            problems.extend(xtask::do27_ports::validate(&do27));
            problems.extend(xtask::do27_ports::check_files(&root, &do27));
            let absorb = notices::load_absorb(&root)?;
            problems.extend(notices::validate_absorb(&root, &absorb));
            if !problems.is_empty() {
                eprintln!("{} has {} problem(s):", notices::PORTED_SOURCES, problems.len());
                for problem in &problems {
                    eprintln!("  {problem}");
                }
                return Ok(ExitCode::FAILURE);
            }
            let asset_rows = notices::load_assets(&root)?;
            let asset_files = notices::tracked_assets(&root)?;
            let mut assets = notices::check_assets(&asset_rows, &asset_files);
            // ADR-8888: every shipping third-party asset is named in the notices.
            let (asset_notices, notice_problems) = notices::resolve_asset_notices(
                &root,
                &asset_rows,
                &notices::load_asset_notices(&root)?,
                &ported,
                &asset_files,
            );
            assets.problems.extend(notice_problems);
            for warning in &assets.warnings {
                eprintln!("warning: {warning}");
            }
            if !assets.problems.is_empty() || (strict && !assets.warnings.is_empty()) {
                eprintln!(
                    "{} has {} problem(s):",
                    notices::ASSET_LICENCES,
                    assets.problems.len()
                );
                for problem in &assets.problems {
                    eprintln!("  {problem}");
                }
                if strict && !assets.warnings.is_empty() {
                    eprintln!(
                        "  --strict: {} file(s) have licence `unknown`",
                        assets.warnings.len()
                    );
                }
                return Ok(ExitCode::FAILURE);
            }
            let current = std::fs::read_to_string(&path).unwrap_or_default();
            // ADR-5903: while the seal matches the resolve inputs, the committed table is the
            // resolve, so the check reads it and runs no `cargo tree` (no network, no manifests).
            // Writing always resolves.
            let sealed = if check {
                notices::sealed_crates(&current, &inputs)
            } else {
                None
            };
            let from_seal = sealed.is_some();
            let crates = match sealed {
                Some(crates) => crates,
                None => {
                    let trees = shipped
                        .iter()
                        .map(|name| notices::shipping_tree(&root, name))
                        .collect::<Result<Vec<_>>>();
                    match trees {
                        Ok(trees) => notices::third_party(&trees),
                        Err(e) if check => return Err(e.context(notices::STALE_SEAL_HELP)),
                        Err(e) => return Err(e),
                    }
                }
            };
            let text = notices::render_crates(&crates, &ported, &asset_notices, Some(&inputs));
            // `regia_cli` compiles its own copy in (a crate cannot read the repo root once packaged, ADR-1485).
            let mirror = root.join(notices::CLI_MIRROR);
            let mirror_current = std::fs::read_to_string(&mirror).unwrap_or_default();
            let unsealed = notices::without_seal(&text);
            if current == text && mirror_current == text {
                if from_seal {
                    println!("notices: up to date (sealed crate table, no resolve needed)");
                } else {
                    println!("notices: up to date");
                }
                ExitCode::SUCCESS
            } else if check
                && notices::without_seal(&current) == unsealed
                && notices::without_seal(&mirror_current) == unsealed
            {
                println!(
                    "notices: up to date (resolved: the seal is stale; `cargo xtask notices` refreshes it so \
                     the next check needs no resolve)"
                );
                ExitCode::SUCCESS
            } else if check {
                eprintln!(
                    "THIRD_PARTY_NOTICES.md (or its copy {}) is out of date; run `cargo xtask notices`",
                    notices::CLI_MIRROR
                );
                let (old, new): (Vec<&str>, Vec<&str>) = (current.lines().collect(), text.lines().collect());
                for line in old.iter().filter(|l| !new.contains(l)) {
                    eprintln!("- {line}");
                }
                for line in new.iter().filter(|l| !old.contains(l)) {
                    eprintln!("+ {line}");
                }
                ExitCode::FAILURE
            } else {
                // Both files atomically, and the first stays as it was if the mirror cannot be written.
                notices::write_pair_atomic(&path, &mirror, &text)?;
                println!("notices: written");
                ExitCode::SUCCESS
            }
        }
        Cmd::DemoNotices { check, verify } => {
            let tree = Graph::demo_tree(&root)?;
            let mut problems = demo_legal::privacy_problems(&tree);
            let eula = std::fs::read_to_string(root.join(demo_legal::EULA_PATH)).unwrap_or_default();
            problems.extend(demo_legal::eula_problems(&eula));
            let ported = notices::load_ported(&root)?;
            // ADR-8888: the bundled fonts and assets the demo's crates compile in.
            let (asset_notices, notice_problems) = notices::resolve_asset_notices(
                &root,
                &notices::load_assets(&root)?,
                &notices::load_asset_notices(&root)?,
                &ported,
                &notices::tracked_assets(&root)?,
            );
            problems.extend(notice_problems);
            let text = demo_legal::render(
                &tree,
                &demo_legal::ported_in_tree(&tree, &ported),
                &demo_legal::notices_in_tree(&tree, &asset_notices),
            );
            // `--verify DIR` is standalone: it compares the package against the freshly rendered
            // text and never writes the repository file (book/user-guide/demo-legal.md).
            if let Some(dir) = &verify {
                problems.extend(demo_legal::verify_package(dir, &text)?);
                if problems.is_empty() {
                    println!("demo-notices: {} is a clean package", dir.display());
                    return Ok(ExitCode::SUCCESS);
                }
                eprintln!("demo-notices: {} problem(s):", problems.len());
                for problem in &problems {
                    eprintln!("  {problem}");
                }
                return Ok(ExitCode::FAILURE);
            }
            let path = root.join(demo_legal::NOTICES_PATH);
            let current = std::fs::read_to_string(&path).unwrap_or_default();
            if !problems.is_empty() {
                eprintln!("demo-notices: {} problem(s):", problems.len());
                for problem in &problems {
                    eprintln!("  {problem}");
                }
                return Ok(ExitCode::FAILURE);
            }
            if current == text {
                println!("demo-notices: up to date");
                ExitCode::SUCCESS
            } else if check {
                eprintln!(
                    "{} is out of date; run `cargo xtask demo-notices`",
                    demo_legal::NOTICES_PATH
                );
                ExitCode::FAILURE
            } else {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                std::fs::write(&path, &text).with_context(|| format!("writing {}", path.display()))?;
                println!("demo-notices: written");
                ExitCode::SUCCESS
            }
        }
        Cmd::BuildTimes { no_clean } => {
            build_times::run(&root, !no_clean)?;
            ExitCode::SUCCESS
        }
        Cmd::CheckExports { lib } => exports::run(&root, lib.as_deref())?,
        Cmd::AndroidLifecycle(args) => android_lifecycle::run(&args)?,
        Cmd::AndroidPass(args) => android_pass::run(&args, &root)?,
        Cmd::WindowsPass(args) => windows_pass::run(&args, &root)?,
        Cmd::Soak(args) => return soak::run(&args, &root),
        Cmd::Budgets { command } => return soak::budgets_command(&command, &root),
        Cmd::BenchDo27 { args } => {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let status = std::process::Command::new(cargo)
                .current_dir(&root)
                .args(["run", "--release", "-p", "lookdev", "--bin", "do27_bench", "--"])
                .args(&args)
                .status()
                .context("running cargo")?;
            if status.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Cmd::WebPlayer(args) => {
            web_player::run(&root, &args)?;
            ExitCode::SUCCESS
        }
        Cmd::FetchTestAssets => {
            fetch_test_assets::run(&root)?;
            ExitCode::SUCCESS
        }
        Cmd::PlateFloors(args) => return plate_floors::run(&root, &args),
        Cmd::GoldenSheets(args) => return golden_sheets::run(&root, &args),
        Cmd::EditorIcons(args) => return xtask::editor_icons::run(&root, &args),
        Cmd::BookCheck { fences: true } => return book::run_fences(&root),
        Cmd::BookCheck { fences: false } => {
            let status = book::run(&root)?;
            if status != ExitCode::SUCCESS {
                return Ok(status);
            }
            return xtask::provenance::run(&root).map_err(Into::into);
        }
        Cmd::BookCoverage { json } => return book_coverage::run(&root, json),
        Cmd::Preflight(args) => return preflight::run(&root, &args),
        Cmd::CheckFeatures(args) => return feature_matrix::run(&root, &args),
        Cmd::AndroidBudgets(args) => return android_pass::budgets::run(&args, &root),
        Cmd::ApiSnapshot(args) => return api_snapshot::run(&root, &args),
        Cmd::AgentSnapshot(args) => return agent_snapshot::run(&root, &args),
        Cmd::DocExamples(args) => return xtask::doc_examples::run(&root, &args),
        Cmd::BakeCheck(args) => return bake_check::run(&args, &root),
        Cmd::ShuffleCheck(args) => return shuffle_check::run(&root, &args),
        Cmd::ReproCheck(args) => return repro_check::run(&root, &args),
        Cmd::EditorMonkey(args) => return editor_monkey::run(&root, &args),
        Cmd::ReplaySoak(args) => return replay_soak::run(&root, &args),
        Cmd::AdrIndex(args) => return adr_index::run(&root, &args),
        Cmd::ReleaseNotes(args) => return release_notes::run(&root, &args),
        Cmd::AgentCoverage => return agent_coverage::run(&root),
        Cmd::EditorAgentCoverage { update } => return xtask::editor_agent_coverage::run(&root, update),
        Cmd::AgentBench(args) => return agent_bench::run(&root, &args),
        Cmd::AgentExamples { print_baseline } => return agent_examples::run(&root, print_baseline),
        Cmd::Manual { check, no_mdbook } => {
            return manual::run(&root, manual::ManualOptions { check, no_mdbook });
        }
        Cmd::CheckEcology => return check_ecology::run(&root),
        Cmd::CheckFormats { update, since_tag } => {
            return check_formats::run(&root, update, since_tag.as_deref());
        }
        Cmd::PerfSmoke(args) => return perf_smoke::run(&args, &root),
        Cmd::PerfBudgets(args) => return perf_budgets::run(&args, &root),
        Cmd::TemplatesCheck(args) => return templates_check::run(&root, &args),
        Cmd::BinarySize(args) => return binary_size::run(&root, &args),
        Cmd::HostStrings(args) => return host_strings::run(&root, &args),
        Cmd::NetMatch(args) => return net_match::run(&root, &args),
        Cmd::NetSoak(args) => return net_soak::run(&root, &args),
        Cmd::CheckProvenance => return xtask::provenance::run(&root).map_err(Into::into),
        Cmd::Walkthrough(args) => return walkthrough::run(&root, &args),
        Cmd::CheckUnwraps => return check_unwraps::run(&root),
        Cmd::DetScan(args) => return xtask::det_scan::run_args(&root, &args),
        Cmd::CheckPackaging => return xtask::packaging::run(&root),
        Cmd::RenameBrand(args) => return rename_brand::run(&args, &root),
        Cmd::ReleaseLedger(args) => return xtask::release_ledger::run(&root, &args),
        Cmd::Smoke(args) => return smoke::run(&args, &root),
        Cmd::EditorSmoke(args) => return xtask::editor_smoke::run(&args, &root),
        Cmd::StreamBench(args) => return stream_bench::run(&root, &args),
        Cmd::Coverage(args) => return coverage::run(&root, &args),
        Cmd::Mutants(args) => return mutants::run(&root, &args),
        Cmd::PlayerSweep(args) => return player_sweep::run(&args, &root),
        Cmd::NightlySweep(args) => return sweep::run(&args, &root),
        Cmd::GiCompare(args) => return gi_compare::run(&args, &root),
        Cmd::GpuBench(args) => return gpu_bench::run(&args, &root),
        Cmd::ShowcaseTable(args) => return showcase_table::run(&args, &root),
        Cmd::Minidump { path, symbols } => return xtask::minidump::run(&path, symbols.as_deref()),
        Cmd::AlphaTriage(args) => alpha_triage::run(&args)?,
        Cmd::Dist(args) => return xtask::dist::run(&args, &root),
        Cmd::DemoPackage(args) => return xtask::demo_package::run(&args, &root),
        Cmd::SteamDemoUpload(args) => return xtask::steam_demo_upload::run(&args, &root),
        Cmd::Flaky(args) => return ci_registry::run_flaky(&root, &args),
        Cmd::TwoTagUpgrade(args) => return two_tag::run_job(&root, &args),
        Cmd::Nightly(args) => return ci_registry::run_nightly(&root, &args, &subcommand_names()),
        Cmd::Fuzz(args) => return fuzz::run(&root, &args),
        Cmd::TestCosts(args) => return xtask::test_costs::run(&root, &args),
        Cmd::FlashCheck(args) => return flash_check::run(&args),
        Cmd::CheckShaders(args) => {
            if let Err(err) = regia_render::validate_engine_shaders() {
                eprintln!("check-shaders: {err}");
                return Ok(ExitCode::FAILURE);
            }
            // The plain modules that pass the layout scan; `PLAIN_LAYOUT_FAILURES` keep their DX12,
            // Metal and contract steps below (ADR-5802).
            let owned = xtask::dx12_shaders::plain_vulkan_sources();
            let borrowed: Vec<(&str, &str)> = owned
                .iter()
                .map(|(name, source)| (name.as_str(), source.as_str()))
                .collect();
            if let Err(err) = regia_render::validate_wgsl_modules(&borrowed) {
                eprintln!("check-shaders: {err}");
                return Ok(ExitCode::FAILURE);
            }
            // The composed modules that pass the layout scan take the Vulkan step too: since ADR-7879
            // that is `pt.wgsl` and `lightmap_bake.wgsl` (`COMPOSED_LAYOUT_FAILURES` is empty).
            let composed = xtask::dx12_shaders::composed_vulkan_sources();
            if !composed.is_empty()
                && let Err(err) = regia_render::validate_composed_modules(&composed)
            {
                eprintln!("check-shaders: {err}");
                return Ok(ExitCode::FAILURE);
            }
            // DX12 (ADR-367): naga's HLSL writer, the Direct3D 12 compute limits and FXC's loop
            // rule over the other crates' shaders (regia_render checks its own in its tests).
            let dx12 = xtask::dx12_shaders::problems();
            for problem in &dx12 {
                eprintln!("check-shaders: dx12: {problem}");
            }
            // Metal (ADR-2644): naga's MSL writer over the same list.
            let msl = xtask::dx12_shaders::msl_problems();
            for problem in &msl {
                eprintln!("check-shaders: msl: {problem}");
            }
            // Rust and WGSL struct agreement of every engine crate's shaders (ADR-3002).
            let contract = match xtask::shader_contract::report() {
                Ok(report) => report,
                Err(err) => {
                    eprintln!("check-shaders: contract: {err}");
                    return Ok(ExitCode::FAILURE);
                }
            };
            for problem in contract.problems() {
                eprintln!("check-shaders: contract: {problem}");
            }
            eprintln!(
                "check-shaders: contract: {} bound structs, {} checked against their Rust type ({} row checks), {} allowlisted",
                contract.bound, contract.covered, contract.checks, contract.allowed
            );
            let mut ok = dx12.is_empty() && msl.is_empty() && contract.ok();
            if args.matrix || args.json {
                ok &= run_shader_matrix(&args, &contract)?;
            }
            if ok {
                if args.json {
                    eprintln!("check-shaders: ok");
                } else {
                    println!("check-shaders: ok");
                }
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Cmd::AndroidCfg => return xtask::android_cfg::run(&root),
        Cmd::LocLength(args) => return xtask::loc_length::run(&args),
        Cmd::DocsLinks => return xtask::docs_links::run(&root),
        Cmd::LlmsTxt(args) => return xtask::llms_txt::run(&root, &args),
        Cmd::OnboardingLint => return xtask::onboarding_lint::run(&root),
        Cmd::PackageSmoke(args) => return xtask::package_smoke::run(&args, &root),
        Cmd::DetGolden(args) => return xtask::det_golden::run(&root, &args),
        Cmd::PublicExport(args) => return xtask::public_export::run(&root, &args),
    })
}

/// `check-shaders` options (ADR-2644).
#[derive(clap::Args, Debug, Clone, Default)]
struct CheckShadersArgs {
    /// Also run the shader permutation matrix (every preset, platform and permutation).
    #[arg(long)]
    matrix: bool,
    /// Print the matrix report as JSON on stdout (runs the matrix; other output goes to stderr).
    #[arg(long)]
    json: bool,
    /// Matrix: only the fixed subset the default test runs.
    #[arg(long)]
    subset: bool,
    /// Matrix: skip the pipeline backend and the per-preset frames (no device needed).
    #[arg(long)]
    no_pipelines: bool,
    /// Matrix: also write the JSON report to this file.
    #[arg(long, value_name = "FILE")]
    json_out: Option<PathBuf>,
}

/// Runs the shader matrix for `check-shaders --matrix`; `Ok(true)` when it passed.
fn run_shader_matrix(args: &CheckShadersArgs, contract: &regia_render::ContractReport) -> Result<bool> {
    let scope = if args.subset {
        regia_render::MatrixScope::Subset
    } else {
        regia_render::MatrixScope::Full
    };
    let mut options = regia_render::ShaderMatrixOptions::new(scope);
    if args.no_pipelines {
        options.pipelines = false;
        options.frames = false;
    }
    let mut report = match regia_render::run_shader_matrix(&options) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("check-shaders: matrix: {err}");
            return Ok(false);
        }
    };
    eprintln!("{}", report.summary());
    for failure in report.failures() {
        eprintln!("check-shaders: matrix: {failure}");
    }
    // The binding counts of every preset against the device-class limits (ADR-3002): a Mobile
    // preset over the `mobile` class fails; Desktop presets report their headroom.
    let limits = xtask::shader_contract::limits(scope);
    for preset in &limits.presets {
        eprintln!(
            "check-shaders: limits: {} ({}, {} modules): {}",
            preset.preset,
            preset.class,
            preset.modules,
            preset
                .worst
                .iter()
                .map(|m| format!("{} {}/{}", m.metric, m.used, m.limit))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    for failure in &limits.failures {
        eprintln!("check-shaders: limits: {failure}");
    }
    let limits_ok = limits.ok();
    report.contract = Some(contract.summary());
    report.limits = Some(limits);
    if args.json || args.json_out.is_some() {
        let json = report.to_json().context("serializing the shader matrix report")?;
        if let Some(path) = &args.json_out {
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
            std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        }
        if args.json {
            println!("{json}");
        }
    }
    Ok(report.passed() && limits_ok)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
