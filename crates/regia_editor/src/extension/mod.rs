// SPDX-License-Identifier: MPL-2.0
//! The editor side of extensions (`docs/specs/57-plugin-extension-api.md` § 3.1, ADR-1885):
//! dynamic library loading behind the ABI stamp, the context editor-audience code registers
//! importers, panels and inspector customizations through, `plugin.reload`, and the Extensions
//! panel.
//!
//! - [`load`]: reads and compares the stamp, copies the library to a unique name, never unloads.
//! - [`context`]: [`ToolExtensionContext`] and the [`EditorExtensions`] registries.
//! - [`host`]: [`EditorExtensionPlugin`] (startup) and [`host::reload`] (a live swap).
//! - [`panel`]: the Extensions panel, over the same functions the `plugin.*` methods run.
//! - [`code_signature`]: the Mach-O code-signature page check run before the loader opens a copy.
//! - [`actions`]: menu and toolbar actions and asset-type tools, and the `editor.extension_*`
//!   agent methods over everything an extension adds to the editor (ADR-9850); palette commands,
//!   shortcuts and selection predicates (ADR-7791).
//! - [`settings`]: typed extension settings in Preferences and Project Settings (ADR-7791).
//! - [`viewport_tools`]: viewport tools with handles and undo, and viewport overlays (ADR-7791).
//! - [`hooks`]: before-cook, after-package and before-launch hooks (ADR-7791).
//!
//! In-process extension code is trusted native code with the editor's full privileges. Nothing
//! here is a sandbox (spec § 2.7).

pub mod actions;
pub mod code_signature;
pub mod context;
pub mod hooks;
pub mod host;
pub mod load;
pub mod panel;
pub mod settings;
pub mod viewport_tools;

pub use actions::{ActionDesc, ActionRun, AssetToolDesc, AssetToolInput, AssetToolRun};
pub use context::{DockRegion, EditorExtensions, InspectorCustomization, PanelDesc, ToolExtensionContext};
pub use host::{DynamicExtensions, EditorExtensionPlugin};
pub use load::{ExtensionLoadError, LoadedExtension, expected_stamp, load_extension};
