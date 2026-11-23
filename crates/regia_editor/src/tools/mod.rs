// SPDX-License-Identifier: MPL-2.0
//! Editor tools (ADR-1523, `book/agents/editor-tools.md`): out-of-process programs the editor
//! launches from a manifest, each with a scoped token of its own, a tab in the Tools panel
//! (`Panel::Tool`), progress events and methods agents call through `editor.tool_call`.
//!
//! - [`manifest`]: the `<id>.tool.toml` format and where manifests are found.
//! - [`host`]: the launcher and runtime state ([`host::ToolHost`]).
//! - [`agent`]: the `editor.tool_*` protocol methods and the per-frame [`agent::tick_tools`].
//! - [`panel`]: the egui panel.

pub mod agent;
pub mod host;
pub mod manifest;
pub mod panel;

pub use agent::{launch_tool, tick_tools};
pub use host::ToolHost;
pub use manifest::ToolManifest;
