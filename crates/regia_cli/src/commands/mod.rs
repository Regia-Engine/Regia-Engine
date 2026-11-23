// SPDX-License-Identifier: MPL-2.0
//! Command implementations. `main.rs` only parses arguments and dispatches here, so the editor can
//! call the same functions later (`docs/specs/21-tooling-and-packaging.md` § 1).

pub mod agent;
pub mod apk_size;
pub mod assets_budget;
pub mod assets_font_subset;
pub mod assets_ids;
pub mod assets_import_rules;
pub mod assets_query;
pub mod assets_texture_sets;
pub mod assets_trust;
pub mod assets_validate;
pub mod atlas;
pub mod bake_gi;
pub mod bake_lightmaps;
pub mod bench;
pub mod build_profile;
pub mod clean;
pub mod content;
pub mod cook;
pub use regia_cli::dcc_feed;
pub mod doctor;
pub mod import;
pub mod install_android;
pub mod level;
pub mod loc;
pub mod loc_check_fit;
pub mod loc_cues;
pub mod loc_exchange;
pub mod loc_sheet;
pub mod loc_vo;
pub mod mcp;
// `regia mcp --http` (ADR-2346).
pub mod mcp_http;
// `regia net desync-replay` (spec 120 § 7.6, ADR-8455).
pub mod net;
pub mod new_project;
pub mod package;
pub mod package_appimage;
pub mod package_sbom;
pub mod plugin;
pub mod prepare;
pub mod preview;
pub mod procgen;
pub mod render;
// The shipping review gate of `regia cook --quality final` (ADR-2346).
pub mod review_gate;
pub mod run;
pub mod server;
pub mod server_content;
pub mod server_package;
pub mod ship_check;
pub mod steam;
pub mod stream_report;
pub mod table;
pub mod test;
pub mod tiled_autotile;
pub mod trace;
pub mod upgrade_project;
// `regia fracture`: pre-fracture a mesh and cook its destruction chunk asset (ADR-1597).
pub mod fracture;
// `regia engine info | package | verify`: engine releases and the version policy (spec 94, ADR-1852).
pub mod engine;
// Scene-embedded component formats for `regia upgrade-project` (post-volume, physical-camera; ADR-2323).
pub(crate) mod upgrade_components;
// `regia editor [--headless]` and `regia gen list | run | verify` (specs/21 § 1, specs/23 § 2 and § 6, ADR-2412).
pub mod editor;
pub mod r#gen;
// `regia viewer pack | verify | standin`: product viewer packages (book agents/viewer-pack.md, ADR-2540).
pub mod viewer;
// `regia describe` (the project map) and `regia preflight` (the kind lint; spec 101, ADR-2488).
pub mod describe;
// `regia export glb`: a level, a prefab or some entities as a `.glb` (ADR-2554).
pub mod export;
// `regia media encode`: AV1 and Opus in WebM with Regia's own encoder, checked by the cook's validator (ADR-3369).
pub mod golden;
pub mod media_encode;
// `regia bake-captures`: local reflection capture bakes (`.fcap`, ADR-2860).
pub mod bake_captures;

// `--checkpoint` / `--resume` of `regia bake-gi` and `regia bake-lightmaps` (ADR-3095).
pub mod bake_checkpoint;
// `regia assets pak-diff <old> <new>`: the update-cost report (spec 22 T2.14, ADR-3322).
pub mod assets_pak_diff;
// `regia export usda`: a USDA stage from a glTF binary (ADR-4604).
pub mod export_usda;
// `regia hooks install|uninstall|status`: the pre-commit hook of `assets validate --staged` (ADR-7789).
pub mod hooks;
// `regia assets contract-check`: a vendor report of one delivery contract (ADR-8450).
pub mod assets_contract;

// glTF source material lint and opt-in metadata fixes (ADR-8061).
pub mod assets_lint_materials;
// `regia terrain passages import|export`: DO27's `do27-tunnels/1` and `<map>.passages.ron` (ADR-8112).
pub mod terrain;
// `regia play-gate replay|check`: a key track against a log (ADR-8403).
pub mod play_gate;
// `regia package parity`: dev trace versus package trace (ADR-8428).
pub mod package_parity;

pub mod logs;
// glTF source geometry lint through the cook evidence cache (ADR-8390).
pub mod assets_lint_geometry;
// `regia engine chunk plan|cut|pack|verify`: release chunks under a byte cap (ADR-8404).
pub mod release_chunk;
// `regia contact-sheet`: a labeled JPEG grid and a luma CSV (ADR-8036).
pub mod contact_sheet;
// `regia rules check`: `.rules.ron` rule sets, checked and resolved (ADR-8694).
pub mod rules;
// `regia assets provenance-import`: pack provenance records into `.meta` provenance (ADR-8834).
pub mod assets_provenance_import;
// `GLIBC_` version needs read from ELF bytes, the binutils-free glibc floor check (ADR-8835).
pub mod elf_versions;
// `regia package --container`: Linux builds inside a pinned build container (C-26, ADR-8835).
pub mod package_container;
// `regia release verify|diff|upload-plan` (ADR-8557).
pub mod release;
// `regia server results`: the finished-match records a host wrote (audit gap GR-10.4).
pub mod server_results;
// `regia editor install-launcher`: the editor in the desktop's app launcher (ADR-20000).
pub mod editor_launcher;
