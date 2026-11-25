// SPDX-License-Identifier: MPL-2.0
//! Repository automation for Regia Engine, as a library so integration tests can exercise the commands
//! (the `xtask` binary in `main.rs` is a thin CLI over this).
//!
//! - [`adr_index`]: `cargo xtask adr-index` — `docs/decisions-index.md` from `docs/decisions.md`.
//! - [`agent_coverage`]: every dispatched agent method has a schema, guide topic, coverage row and test.
//! - [`book_coverage`]: host extension methods, Cargo features and CLI commands each have a book page.
//! - [`layers`]: crate DAG and runtime exclusions (`docs/02-architecture.md` § 1, `docs/06` § 7).
//! - [`graph`]: `cargo metadata` wrapper used by the checks.
//! - [`soak`]: `cargo xtask soak` — long-run stability sessions with budgets (`docs/06` § 4, M18).
//! - [`golden_sheets`]: `cargo xtask golden-sheets` — contact sheets of golden PNGs that changed
//!   between two revisions.
//! - [`minidump`]: `cargo xtask minidump` — the crashing thread's stack from a native crash bundle.
//! - [`smoke`]: `cargo xtask smoke` — engine side of the DO27 alpha smoke list.
//! - [`preflight`]: `cargo xtask preflight` — the merge gate's cheap checks in gate order.
//! - [`player_sweep`]: `cargo xtask player-sweep` — cold launch, replay, quits and a crash test.
//! - [`bake_check`]: `cargo xtask bake-check` — baked light end to end on the fixture project.
//! - [`gi_compare`]: `cargo xtask gi-compare` — baked light against the path tracer, per view, with
//!   contact sheets and a `summary.json` (ADR-314).
//! - [`dist`]: `cargo xtask dist` — Windows and Steam Deck test bundles with a manifest (ADR-868).
//! - [`ci_registry`]: `cargo xtask flaky` / `nightly` — the flaky-test and nightly-job registries.
//! - [`replay_soak`]: `cargo xtask replay-soak` — record/replay state-hash traces across thread counts and builds.
//! - [`rename_brand`]: `cargo xtask rename-brand` — the deterministic rename to Regia Engine (ADR-1388).
//! - [`binary_size`]: `cargo xtask binary-size` — shipped binary size by crate against `docs/ci/size-budgets.toml` (ADR-3188).
//! - [`templates_check`]: `cargo xtask templates-check` — every project template generated, built and smoked (ADR-2046).
//! - [`walkthrough`]: `cargo xtask walkthrough` — the book's getting-started path run as a stranger (ADR-2954).
//! - [`gpu_bench`]: `cargo xtask gpu-bench` — bindless, GI, baked-light and feature-cost bench suites on one card
//!   ([`gpu_bench_features`]).
//! - [`api_snapshot`]: `cargo xtask api-snapshot` — the stable crates' public API against `docs/api-snapshots/`.
//! - [`agent_snapshot`]: `cargo xtask agent-snapshot` — the agent protocol surface against
//!   `docs/api-snapshots/agent-protocol.txt`, with the breaking/additive ratchet (ADR-3335).
//! - [`feature_matrix`]: `cargo xtask check-features` — the optional Cargo features the default build never compiles (ADR-2322).
//! - [`net_match`]: `cargo xtask net-match` — the `net-game` template played across real machines (ADR-3075).
//! - [`release_ledger`]: `cargo xtask release-ledger` — the v0.5.0 release evidence ledger's commits, evidence and integration claims (ADR-4935).
//! - [`package_smoke`]: `cargo xtask package-smoke` — package, play a scripted input recording, and
//!   fail on an `ERROR` log record or a dirty `summary.json` (ADR-3968).

pub mod adr_index;
pub mod agent_bench;
pub mod agent_coverage;
pub mod agent_examples;
pub mod agent_snapshot;
pub mod alpha_triage;
pub mod android_lifecycle;
pub mod android_pass;
pub mod api_snapshot;
pub mod bake_check;
pub mod book;
pub mod book_coverage;
pub mod build_times;
pub mod check_ecology;
pub mod check_formats;
pub mod check_unwraps;
pub mod ci_registry;
pub mod coverage;
pub mod demo_legal;
pub mod demo_package;
pub mod dist;
pub mod dist_archive;
pub mod dist_binary;
pub mod duplicate_deps;
pub mod dx12_shaders;
pub mod editor_agent_coverage;
pub mod exports;
pub mod feature_matrix;
pub mod fetch_test_assets;
pub mod gi_compare;
pub mod golden_sheets;
pub mod gpu_bench;
pub mod gpu_bench_features;
pub mod graph;
pub mod host_strings;
pub mod layers;
pub mod manual;
pub mod markdown;
pub mod minidump;
pub mod mutants;
pub mod notices;
pub mod packaging;
pub mod perf_budgets;
pub mod perf_smoke;
pub mod plate_floors;
pub mod player_sweep;
pub mod preflight;
pub mod release_notes;
pub mod rename_brand;
pub mod replay_soak;
pub mod repro_check;
pub mod sbom;
pub mod showcase_table;
pub mod shuffle_check;
pub mod smoke;
pub mod soak;
pub mod stream_bench;
pub mod sweep;
pub mod templates_check;
// `cargo xtask binary-size`: shipped-binary size by crate, with budgets (ADR-3188).
pub mod binary_size;

pub mod web_player;
// `cargo xtask check-kinds`: the kind lint over the samples (spec 101, ADR-2488).
pub mod android_cfg;
pub mod check_kinds;
pub mod docs_links;
pub mod flash_check;
pub mod package_smoke;
pub mod shader_contract;
pub mod test_costs;
pub mod two_tag;
pub mod walkthrough;
pub mod windows_pass;
// `cargo xtask net-match`: the net-game template across real machines (ADR-3075).
pub mod net_match;
// `cargo xtask steam-demo-upload`: stage the demo package for steamcmd and print the command (ADR-4899).
pub mod steam_demo_upload;
// `cargo xtask release-ledger`: the v0.5.0 release evidence ledger checker (ADR-4935).
pub mod release_ledger;
// `cargo xtask check-provenance`: source classes and public-page citations (ADR-4581).
pub mod provenance;
// `cargo xtask loc-length`: pseudo-localization length budgets (ADR-4564).
pub mod loc_length;
// `cargo xtask doc-examples`: the ratchet on compiled documentation examples of the stable crates.
pub mod doc_examples;
// `cargo xtask editor-monkey`: the editor monkey test at a chosen size (w11 item 4).
pub mod editor_monkey;
// `cargo xtask editor-smoke`: drive every editor panel, menu item, command and action headless (ADR-7865).
pub mod editor_smoke;
// `[[do27]]` rows of `docs/ported-sources.toml`, checked by `notices --check` (ADR-8012).
pub mod do27_ports;
// `cargo xtask det-scan`: the lockstep determinism scan over `docs/ci/lockstep-closure.toml` (ADR-8013).
pub mod det_scan;
// `cargo xtask check-formats` also checks designed, unbuilt formats against spec 12 § 4a (ADR-8019).
pub mod proposed_formats;
// `cargo xtask det-golden`: the cross-target runners of the determinism goldens (ADR-8081).
pub mod det_golden;
// `cargo xtask net-soak`: regia_net under impaired networks, with bars (ADR-7899).
pub mod net_soak;
// `cargo xtask check-net-core`: the ECS-free regia_net core builds, links no ECS and runs its example (ADR-8501).
pub mod net_core;
// `cargo xtask editor-icons`: the Regia Editor's app icons from one SVG (ADR-20000).
pub mod editor_icons;
// `cargo xtask spdx`: the SPDX licence line on every first-party .rs/.wgsl file (ADR-10225).
pub mod spdx;
// `cargo xtask llms-txt`: the book's index and full text for AI assistants (ADR-10135).
pub mod llms_txt;
// `cargo xtask onboarding-lint`: the user onboarding files against the onboarding contract and the CLI (ADR-10135, ADR-10230).
pub mod onboarding_lint;
// `cargo xtask public-export`: the public repository's single-commit snapshot and its leak check (ADR-10229).
pub mod public_export;
// `cargo xtask manual` navigation pages: index, glossary and feature catalog (ADR-10418).
pub mod manual_nav;
