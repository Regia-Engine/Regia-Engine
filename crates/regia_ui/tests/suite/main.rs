// SPDX-License-Identifier: MPL-2.0
//! One test binary for the integration tests of `regia_ui` that need no binary of their own (ADR-2986,
//! `docs/06-testing-and-ci.md` § 9): every module below was a `tests/<name>.rs` binary, and each test now
//! runs as `<name>::<test>`. Run one with `cargo test -p regia_ui --test suite <name>::<test>`.
//!
//! A new test file still goes at `tests/<name>.rs` as its own binary by default. Move it here only when it
//! needs no process of its own: no `#[global_allocator]`, no environment or working-directory change, no
//! process-wide singleton, no GPU device, and no doc, script or registry that names its binary.

mod gamepad_nav;
mod handheld_scale;
mod input_layers;
mod layout_semantics;
mod list_virtual;
mod motion_presets;
mod popup_menus;
mod public_behavior;
mod radial_wheel;
mod rebind_panel_options;
mod scroll_containers;
mod spec18_conformance;
mod theme_switch;
mod theme_transition;
mod tq_color_ops;
mod tq_draw;
mod tq_ease;
mod tq_icons;
mod tq_list_math;
mod tq_loc_manifest;
mod tq_localization_edges;
mod tq_popup_radial;
mod tq_scroll_math;
mod tq_seams;
mod tq_shortcuts;
mod tq_style;
mod tq_text_input_api;
mod tq_tooltip_machine;
mod v11_designer;
mod view_models;
