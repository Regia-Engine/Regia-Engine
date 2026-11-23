// SPDX-License-Identifier: MPL-2.0
//! The `.editorpanel.ron` sidecar (format `editor-panel`, version 1, ADR-8189): names the UI
//! document a utility panel draws, the driver that answers its commands and the tab title. See
//! `docs/formats/editor-panel-ron.md`.
//!
//! ```ron
//! (
//!     format_version: 1,
//!     title: "Rename selection",
//!     layout: "rename_with_prefix.ui.ron",
//!     driver: Lua("rename_with_prefix.lua"),
//! )
//! ```
//!
//! Paths are relative to the sidecar's folder, use `/`, and may not leave it (`..` and absolute
//! paths are refused): a panel is a folder of project files that can be copied as a whole.

use serde::{Deserialize, Serialize};

/// The current `.editorpanel.ron` version.
pub const EDITOR_PANEL_FORMAT_VERSION: u32 = 1;
/// The file suffix of a utility panel sidecar.
pub const EDITOR_PANEL_SUFFIX: &str = ".editorpanel.ron";
/// The longest title, in characters.
pub const MAX_TITLE_CHARS: usize = 64;
/// The longest relative path, in bytes.
pub const MAX_PATH_BYTES: usize = 256;
/// The smallest and largest panel size, in points per side.
pub const SIZE_RANGE: (f32, f32) = (64.0, 2048.0);
/// The size a sidecar without `size` gets, in points.
pub const DEFAULT_SIZE: [f32; 2] = [360.0, 200.0];

/// What answers the panel's commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PanelDriverDef {
    /// A `.logic.ron` logic graph (`docs/formats/logic-ron.md`): each command is an inbox event.
    Logic(String),
    /// A Lua 5.1 script run in the mod sandbox (`docs/specs/84-lua-mods.md`): each command is a
    /// `command` event.
    Lua(String),
}

impl PanelDriverDef {
    /// The driver file, relative to the sidecar.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Logic(path) | Self::Lua(path) => path,
        }
    }

    /// `logic` or `lua`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Logic(_) => "logic",
            Self::Lua(_) => "lua",
        }
    }
}

/// One `.editorpanel.ron` file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorPanelDef {
    /// [`EDITOR_PANEL_FORMAT_VERSION`] (a missing field reads as the current version).
    #[serde(default = "crate::editor_prefs::version_missing")]
    pub format_version: u32,
    /// The tab title and the Window ▸ Utility Panels entry.
    pub title: String,
    /// The `.ui.ron` document drawn in the panel.
    pub layout: String,
    /// The driver that answers the document's commands.
    pub driver: PanelDriverDef,
    /// The panel's UI size in points (width, height); the document lays out at this size.
    #[serde(default = "default_size")]
    pub size: [f32; 2],
}

fn default_size() -> [f32; 2] {
    DEFAULT_SIZE
}

/// Why a sidecar was refused. `code` is the agent error code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelFileError {
    /// Lowercase agent error code: `utility_panel_parse`, `utility_panel_newer` or
    /// `utility_panel_invalid`.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl std::fmt::Display for PanelFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

fn invalid(message: impl Into<String>) -> PanelFileError {
    PanelFileError {
        code: "utility_panel_invalid",
        message: message.into(),
    }
}

/// Checks one relative path field: non-empty, `/`-separated, inside the sidecar's folder, with
/// the given suffixes.
fn check_path(field: &str, path: &str, suffixes: &[&str]) -> Result<(), PanelFileError> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(invalid(format!("`{field}` must be 1-{MAX_PATH_BYTES} bytes")));
    }
    if path.starts_with('/') || path.contains('\\') || path.contains(':') {
        return Err(invalid(format!(
            "`{field}` {path:?} must be a relative path with `/` separators"
        )));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(invalid(format!(
            "`{field}` {path:?} may not have empty, `.` or `..` parts"
        )));
    }
    if !suffixes.iter().any(|suffix| path.ends_with(suffix)) {
        return Err(invalid(format!(
            "`{field}` {path:?} must end in {}",
            suffixes.join(" or ")
        )));
    }
    Ok(())
}

impl EditorPanelDef {
    /// Checks the fields a parse cannot: the title, the paths and the size.
    ///
    /// # Errors
    /// `utility_panel_invalid` naming the field.
    pub fn check(&self) -> Result<(), PanelFileError> {
        let title = self.title.trim();
        if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
            return Err(invalid(format!("`title` must be 1-{MAX_TITLE_CHARS} characters")));
        }
        if self.title.chars().any(char::is_control) {
            return Err(invalid("`title` may not hold control characters"));
        }
        check_path("layout", &self.layout, &[".ui.ron"])?;
        match &self.driver {
            PanelDriverDef::Logic(path) => check_path("driver", path, &[".logic.ron"])?,
            PanelDriverDef::Lua(path) => check_path("driver", path, &[".lua"])?,
        }
        let (min, max) = SIZE_RANGE;
        if self
            .size
            .iter()
            .any(|side| !side.is_finite() || !(min..=max).contains(side))
        {
            return Err(invalid(format!("`size` sides must be {min}-{max} points")));
        }
        Ok(())
    }

    /// Reads a sidecar (current version; a missing `format_version` reads as it).
    ///
    /// # Errors
    /// `utility_panel_parse` (not the shape, or version 0), `utility_panel_newer` (a newer
    /// version than this editor reads), `utility_panel_invalid` ([`check`](Self::check)).
    pub fn from_text(text: &str) -> Result<Self, PanelFileError> {
        let mut def = crate::editor_prefs::read_versioned(text, EDITOR_PANEL_FORMAT_VERSION, |d: &Self| {
            d.format_version
        })
        .map_err(|e| match e {
            crate::editor_prefs::VersionedError::Parse(message) => PanelFileError {
                code: "utility_panel_parse",
                message,
            },
            crate::editor_prefs::VersionedError::Newer(found) => PanelFileError {
                code: "utility_panel_newer",
                message: format!(
                    "written by a newer editor (format_version {found}, this editor reads up to {EDITOR_PANEL_FORMAT_VERSION})"
                ),
            },
        })?;
        def.format_version = EDITOR_PANEL_FORMAT_VERSION;
        def.check()?;
        Ok(def)
    }

    /// The file text (current version).
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut def = self.clone();
        def.format_version = EDITOR_PANEL_FORMAT_VERSION;
        ron::ser::to_string_pretty(&def, regia_core::ron_pretty().struct_names(false)).unwrap_or_default()
    }
}

/// The `from` versions of the registered `.editorpanel.ron` migration steps (none: version 1 is
/// the first).
#[must_use]
pub fn editor_panel_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape of `.editorpanel.ron` (the `editor-panel` format).
#[must_use]
pub fn editor_panel_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<EditorPanelDef>(),
        regia_core::schema::serde_variant_names::<PanelDriverDef>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"(
    format_version: 1,
    title: "Rename selection",
    layout: "rename.ui.ron",
    driver: Lua("rename.lua"),
)"#;

    #[test]
    fn utility_panel_sidecar_reads_writes_and_refuses() {
        let def = EditorPanelDef::from_text(SAMPLE).expect("reads");
        assert_eq!(def.title, "Rename selection");
        assert_eq!(def.driver, PanelDriverDef::Lua("rename.lua".into()));
        assert_eq!(def.size, DEFAULT_SIZE);
        let again = EditorPanelDef::from_text(&def.to_text()).expect("round trip");
        assert_eq!(again, def);
        // A missing version reads as the current one.
        let bare = SAMPLE.replace("format_version: 1,", "");
        assert!(EditorPanelDef::from_text(&bare).is_ok());
        let newer = SAMPLE.replace("format_version: 1", "format_version: 2");
        assert_eq!(
            EditorPanelDef::from_text(&newer).expect_err("refused").code,
            "utility_panel_newer"
        );
        let zero = SAMPLE.replace("format_version: 1", "format_version: 0");
        assert_eq!(
            EditorPanelDef::from_text(&zero).expect_err("refused").code,
            "utility_panel_parse"
        );
        let unknown = SAMPLE.replace("title:", "extra: 1, title:");
        assert_eq!(
            EditorPanelDef::from_text(&unknown).expect_err("refused").code,
            "utility_panel_parse"
        );
        for (from, to) in [
            ("rename.ui.ron", "../rename.ui.ron"),
            ("rename.ui.ron", "/abs/rename.ui.ron"),
            ("rename.ui.ron", "rename.txt"),
            ("rename.lua", "rename.logic.ron"),
            ("\"Rename selection\"", "\"  \""),
        ] {
            let bad = SAMPLE.replace(from, to);
            assert_eq!(
                EditorPanelDef::from_text(&bad).expect_err("refused").code,
                "utility_panel_invalid",
                "{to}"
            );
        }
        let logic = SAMPLE.replace("Lua(\"rename.lua\")", "Logic(\"rename.logic.ron\")");
        assert_eq!(
            EditorPanelDef::from_text(&logic).expect("logic").driver.kind(),
            "logic"
        );
        let tiny = SAMPLE.replace("driver:", "size: (8, 8), driver:");
        assert_eq!(
            EditorPanelDef::from_text(&tiny).expect_err("refused").code,
            "utility_panel_invalid"
        );
    }
}
