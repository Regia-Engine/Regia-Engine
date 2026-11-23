// SPDX-License-Identifier: MPL-2.0
//! Editor preferences (`docs/specs/19-editor.md` § 2 "Editor Preferences", ADR-2321): typed per-user
//! settings with search, reset to default and a versioned file.
//!
//! [`PREFS`] is the registry: every setting has a stable dotted key, a category, a label, a kind
//! (bool, integer or float range, or a fixed choice) and a default equal to the editor's behavior
//! before preferences existed, so an editor with no preferences file looks and acts as it did.
//! [`EditorPreferences`] (a world resource) holds only the values that differ from the defaults;
//! [`EditorPreferences::reset`] drops one, [`EditorPreferences::reset_all`] drops them all.
//!
//! The file is `<user config>/regia/editor_prefs.ron` ([`user_config_dir`]):
//!
//! ```ron
//! (
//!     format_version: 1,
//!     values: { "appearance.ui_scale": Float(1.25), "camera.fly_speed": Float(12.0) },
//! )
//! ```
//!
//! Reading migrates older versions through `migrate_prefs` (version 1 is the first; a file with
//! no `format_version` reads as version 1), refuses a newer version without touching the file, keeps
//! keys this build does not know (a newer editor's settings survive a save by an older one) and
//! drops values of the wrong kind or out of range with a diagnostic, so they fall back to the
//! default. Writes are atomic (temp file, then rename).
//!
//! The editor only reads and writes these files when the host inserted [`EditorConfigPaths`]
//! (the editor binary does, with [`EditorConfigPaths::user`]); a world without it keeps the
//! preferences for the session, which keeps tests hermetic.
//!
//! The agent surface is `editor.prefs_get`, `editor.prefs_set` and `editor.prefs_reset`
//! (`book/agents/editor-preferences.md`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

/// Current version of `editor_prefs.ron`.
pub const EDITOR_PREFS_FORMAT_VERSION: u32 = 1;
/// The preferences file name inside the user config directory.
pub const PREFS_FILE: &str = "editor_prefs.ron";
/// The keymap file name inside the user config directory (`crate::editor_keymap`).
pub const KEYMAP_FILE: &str = "editor_keymap.ron";
/// The user's default dock layout inside the user config directory (`crate::editor_layout`).
pub const USER_LAYOUT_FILE: &str = "editor_layout.ron";

/// One stored value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PrefValue {
    /// A switch.
    Bool(bool),
    /// A whole number.
    Int(i64),
    /// A number.
    Float(f64),
    /// Text (the value of a choice).
    Text(String),
}

impl PrefValue {
    /// The value as JSON.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Bool(b) => serde_json::Value::Bool(*b),
            Self::Int(i) => serde_json::Value::from(*i),
            Self::Float(f) => serde_json::Value::from(*f),
            Self::Text(t) => serde_json::Value::String(t.clone()),
        }
    }

    /// A JSON value as a [`PrefValue`] (`None` for arrays, objects and null).
    #[must_use]
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        match value {
            serde_json::Value::Bool(b) => Some(Self::Bool(*b)),
            serde_json::Value::Number(n) => n.as_i64().map(Self::Int).or_else(|| n.as_f64().map(Self::Float)),
            serde_json::Value::String(s) => Some(Self::Text(s.clone())),
            _ => None,
        }
    }
}

/// What a setting holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PrefKind {
    /// On or off.
    Bool,
    /// A whole number in `min..=max`.
    Int {
        /// Smallest value.
        min: i64,
        /// Largest value.
        max: i64,
    },
    /// A finite number in `min..=max`.
    Float {
        /// Smallest value.
        min: f64,
        /// Largest value.
        max: f64,
    },
    /// One of a fixed list of names.
    Choice(&'static [&'static str]),
    /// Free text of at most `max` bytes, one line.
    Text {
        /// Longest value, bytes.
        max: usize,
    },
}

impl PrefKind {
    /// The kind's wire name: `bool`, `int`, `float`, `choice` or `text`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int { .. } => "int",
            Self::Float { .. } => "float",
            Self::Choice(_) => "choice",
            Self::Text { .. } => "text",
        }
    }
}

/// A setting's default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PrefDefault {
    /// A switch.
    Bool(bool),
    /// A whole number.
    Int(i64),
    /// A number.
    Float(f64),
    /// A choice name.
    Text(&'static str),
}

impl PrefDefault {
    /// The default as a stored value.
    #[must_use]
    pub fn value(self) -> PrefValue {
        match self {
            Self::Bool(b) => PrefValue::Bool(b),
            Self::Int(i) => PrefValue::Int(i),
            Self::Float(f) => PrefValue::Float(f),
            Self::Text(t) => PrefValue::Text(t.to_owned()),
        }
    }
}

/// One registered setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrefDef {
    /// Stable dotted key (`appearance.ui_scale`), the file and agent name.
    pub key: &'static str,
    /// The Preferences window section.
    pub category: &'static str,
    /// The row label.
    pub label: &'static str,
    /// What the setting does.
    pub description: &'static str,
    /// What it holds.
    pub kind: PrefKind,
    /// The value with no preference set.
    pub default: PrefDefault,
}

impl PrefDef {
    /// Checks `value` against the kind and range: an integer is accepted for a float setting.
    ///
    /// # Errors
    ///
    /// A message naming what the setting accepts.
    pub fn check(&self, value: &PrefValue) -> Result<PrefValue, String> {
        if self.key == "agents.commands"
            && let PrefValue::Text(text) = value
            && crate::agent_sessions::sensitive(text)
        {
            return Err(
                "Keep credentials in the external agent's environment, never in Agent commands".into(),
            );
        }
        match (self.kind, value) {
            (PrefKind::Bool, PrefValue::Bool(b)) => Ok(PrefValue::Bool(*b)),
            (PrefKind::Int { min, max }, PrefValue::Int(i)) => {
                if (min..=max).contains(i) {
                    Ok(PrefValue::Int(*i))
                } else {
                    Err(format!("{} takes a whole number from {min} to {max}", self.key))
                }
            }
            (PrefKind::Float { min, max }, PrefValue::Float(_) | PrefValue::Int(_)) => {
                let f = match value {
                    PrefValue::Float(f) => *f,
                    PrefValue::Int(i) => *i as f64,
                    _ => f64::NAN,
                };
                if f.is_finite() && (min..=max).contains(&f) {
                    Ok(PrefValue::Float(f))
                } else {
                    Err(format!("{} takes a number from {min} to {max}", self.key))
                }
            }
            (PrefKind::Choice(choices), PrefValue::Text(t)) => {
                if choices.contains(&t.as_str()) {
                    Ok(PrefValue::Text(t.clone()))
                } else {
                    Err(format!("{} takes one of {}", self.key, choices.join(", ")))
                }
            }
            (PrefKind::Text { max }, PrefValue::Text(t)) => {
                if t.len() <= max && !t.chars().any(char::is_control) {
                    check_text_value(self.key, t)?;
                    Ok(PrefValue::Text(t.clone()))
                } else {
                    Err(format!("{} takes one line of at most {max} bytes", self.key))
                }
            }
            (kind, _) => Err(format!("{} takes a {} value", self.key, kind.name())),
        }
    }

    /// The text the search matches: `Category: Label key description`.
    fn haystack(&self) -> String {
        format!(
            "{}: {} {} {}",
            self.category, self.label, self.key, self.description
        )
    }
}

/// The Theme choices: the editor's own looks (`default` is Piercing Graphite, then AMOLED Night,
/// Paper and the high-contrast theme, `crate::theme::BUILTIN_EDITOR_THEMES`) and the two built-in
/// UI themes.
pub const THEME_CHOICES: &[&str] = &[
    "default",
    "amoled-night",
    "paper",
    "high-contrast",
    "regia-dark",
    "regia-light",
];

/// The preference naming the saved custom theme in use (empty: the Theme choice).
pub const CUSTOM_THEME_PREF: &str = "appearance.custom_theme";
/// The text scale preference (egui text sizes, separate from the whole-UI scale).
pub const TEXT_SCALE_PREF: &str = "appearance.text_scale";
/// The diagnostic color preferences, `#rrggbb` or empty for the theme's color: error, warning,
/// info, success.
pub const DIAGNOSTIC_COLOR_PREFS: [&str; 4] = [
    "appearance.color_error",
    "appearance.color_warning",
    "appearance.color_info",
    "appearance.color_success",
];

/// Whether a change of `key` recomposes the editor theme ([`apply_appearance`]).
fn is_appearance_color_key(key: &str) -> bool {
    key == "appearance.theme" || key == CUSTOM_THEME_PREF || DIAGNOSTIC_COLOR_PREFS.contains(&key)
}

/// Extra checks of text preferences whose kind alone is too loose: a diagnostic color is empty or
/// `#rrggbb`, a custom theme is empty or a valid theme name.
fn check_text_value(key: &str, text: &str) -> Result<(), String> {
    if DIAGNOSTIC_COLOR_PREFS.contains(&key) {
        let ok = text.is_empty()
            || (text.len() == 7 && text.starts_with('#') && text[1..].chars().all(|c| c.is_ascii_hexdigit()));
        if !ok {
            return Err(format!(
                "{key} takes a color \"#rrggbb\", or \"\" for the theme's"
            ));
        }
    }
    if key == CUSTOM_THEME_PREF && !text.is_empty() && !crate::theme::valid_theme_name(text) {
        return Err(format!(
            "{key} takes a saved theme name (letters, digits, spaces, - and _), or \"\" for none"
        ));
    }
    Ok(())
}

/// Every editor preference, in the Preferences window's order.
pub const PREFS: &[PrefDef] = &[
    PrefDef {
        key: "appearance.theme",
        category: "Appearance",
        label: "Theme",
        description: "The editor's colors: Piercing Graphite (default), AMOLED Night, Paper, high contrast, or one of the built-in UI themes.",
        kind: PrefKind::Choice(THEME_CHOICES),
        default: PrefDefault::Text("default"),
    },
    PrefDef {
        key: "appearance.ui_scale",
        category: "Appearance",
        label: "UI scale",
        description: "Zoom of the whole editor interface, on top of the display's scale.",
        kind: PrefKind::Float { min: 0.5, max: 3.0 },
        default: PrefDefault::Float(1.0),
    },
    // Appearance: text scale, saved theme and diagnostic colors (spec addendum § 21, ADR-7838).
    PrefDef {
        key: TEXT_SCALE_PREF,
        category: "Appearance",
        label: "Text scale",
        description: "Size of the editor's text, separate from the UI scale (which also grows spacing and icons).",
        kind: PrefKind::Float { min: 0.8, max: 2.0 },
        default: PrefDefault::Float(1.0),
    },
    PrefDef {
        key: CUSTOM_THEME_PREF,
        category: "Appearance",
        label: "Custom theme",
        description: "A theme saved from the Theme page (themes/<name>.editor_theme.ron in the user config directory); empty uses the Theme choice.",
        kind: PrefKind::Text { max: 64 },
        default: PrefDefault::Text(""),
    },
    PrefDef {
        key: "appearance.color_error",
        category: "Appearance",
        label: "Error color",
        description: "Color of errors in the Output Log, the Problems panel, status lines and badges, as #rrggbb; empty uses the theme's.",
        kind: PrefKind::Text { max: 7 },
        default: PrefDefault::Text(""),
    },
    PrefDef {
        key: "appearance.color_warning",
        category: "Appearance",
        label: "Warning color",
        description: "Color of warnings in the Output Log, the Problems panel, status lines and badges, as #rrggbb; empty uses the theme's.",
        kind: PrefKind::Text { max: 7 },
        default: PrefDefault::Text(""),
    },
    PrefDef {
        key: "appearance.color_info",
        category: "Appearance",
        label: "Info color",
        description: "Color of information lines in the Output Log and the Problems panel, as #rrggbb; empty uses the theme's.",
        kind: PrefKind::Text { max: 7 },
        default: PrefDefault::Text(""),
    },
    PrefDef {
        key: "appearance.color_success",
        category: "Appearance",
        label: "Success color",
        description: "Color of passing checks and valid items, as #rrggbb; empty uses the theme's.",
        kind: PrefKind::Text { max: 7 },
        default: PrefDefault::Text(""),
    },
    PrefDef {
        key: "camera.fly_speed",
        category: "Camera",
        label: "Fly speed",
        description: "The viewport camera's fly speed in meters per second when a session starts (the scroll wheel still changes it).",
        kind: PrefKind::Float { min: 0.1, max: 500.0 },
        default: PrefDefault::Float(6.0),
    },
    PrefDef {
        key: "camera.field_of_view",
        category: "Camera",
        label: "Field of view",
        description: "The viewport camera's vertical field of view in degrees.",
        kind: PrefKind::Float {
            min: 20.0,
            max: 120.0,
        },
        default: PrefDefault::Float(60.0),
    },
    PrefDef {
        key: "viewport.grid",
        category: "Viewport",
        label: "Show grid",
        description: "Whether the viewport grid shows when the editor starts.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "viewport.realtime",
        category: "Viewport",
        label: "Realtime viewport",
        description: "Whether the viewport redraws every frame when the editor starts.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "autosave.interval_minutes",
        category: "Autosave",
        label: "Autosave interval (minutes)",
        description: "How often a level with unsaved edits is autosaved for crash recovery; 0 turns autosave off.",
        kind: PrefKind::Int { min: 0, max: 120 },
        default: PrefDefault::Int(5),
    },
    PrefDef {
        key: "history.keep_autosaves",
        category: "Autosave",
        label: "Autosave versions kept",
        description: "How many autosave versions File ▸ Version History keeps per level and document; 0 keeps none.",
        kind: PrefKind::Int { min: 0, max: 100 },
        default: PrefDefault::Int(10),
    },
    PrefDef {
        key: "history.keep_saves",
        category: "Autosave",
        label: "Saved versions kept",
        description: "How many manual-save versions File ▸ Version History keeps per level and document; 0 keeps none.",
        kind: PrefKind::Int { min: 0, max: 100 },
        default: PrefDefault::Int(10),
    },
    PrefDef {
        key: "history.max_mb",
        category: "Autosave",
        label: "Version history size cap (MB)",
        description: "The most disk space the project's version history may use; the oldest versions go first.",
        kind: PrefKind::Int { min: 1, max: 100_000 },
        default: PrefDefault::Int(512),
    },
    PrefDef {
        key: "layout.restore",
        category: "Layout",
        label: "Restore saved layout",
        description: "Open a project with its saved dock layout (or your default layout) instead of the built-in one.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "layout.save_on_exit",
        category: "Layout",
        label: "Save layout on exit",
        description: "Save the dock layout into the project when the editor closes.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "code.editor_command",
        category: "Code",
        label: "Code editor command",
        description: "What a click on file:line in the Output Log runs: code, zed, $EDITOR, or a command with {file}, {line} and {col}. Empty uses [editor] code_editor of .regia/agent.toml, then $VISUAL and $EDITOR. Agents cannot change it.",
        kind: PrefKind::Text { max: 1024 },
        default: PrefDefault::Text(""),
    },
    PrefDef {
        key: "procgen.hit.path.terrain",
        category: "Procedural tools",
        label: "Path tool lands on terrain",
        description: "Whether the procedural Path tool's cursor ray may hit terrain (spec 67 § 5.3).",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "procgen.hit.path.meshes",
        category: "Procedural tools",
        label: "Path tool lands on meshes",
        description: "Whether the procedural Path tool's cursor ray may hit meshes.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "procgen.hit.path.skip_generated",
        category: "Procedural tools",
        label: "Path tool skips generated output",
        description: "Whether the procedural Path tool ignores entities a generator made, so it never builds on its own output.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "procgen.hit.path.physics_mask",
        category: "Procedural tools",
        label: "Path tool layer mask",
        description: "Render layers (one bit per layer) a mesh must be on for the procedural Path tool to land on it; 4294967295 is every layer.",
        kind: PrefKind::Int {
            min: 0,
            max: 4_294_967_295,
        },
        default: PrefDefault::Int(4_294_967_295),
    },
    PrefDef {
        key: "procgen.hit.area.terrain",
        category: "Procedural tools",
        label: "Area tool lands on terrain",
        description: "Whether the procedural Area tool's cursor ray may hit terrain (spec 67 § 5.3).",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "procgen.hit.area.meshes",
        category: "Procedural tools",
        label: "Area tool lands on meshes",
        description: "Whether the procedural Area tool's cursor ray may hit meshes.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "procgen.hit.area.skip_generated",
        category: "Procedural tools",
        label: "Area tool skips generated output",
        description: "Whether the procedural Area tool ignores entities a generator made, so it never builds on its own output.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "procgen.hit.area.physics_mask",
        category: "Procedural tools",
        label: "Area tool layer mask",
        description: "Render layers (one bit per layer) a mesh must be on for the procedural Area tool to land on it; 4294967295 is every layer.",
        kind: PrefKind::Int {
            min: 0,
            max: 4_294_967_295,
        },
        default: PrefDefault::Int(4_294_967_295),
    },
    PrefDef {
        key: "procgen.hit.paint.terrain",
        category: "Procedural tools",
        label: "Paint tool lands on terrain",
        description: "Whether the procedural Paint tool's cursor ray may hit terrain (spec 67 § 5.3).",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "procgen.hit.paint.meshes",
        category: "Procedural tools",
        label: "Paint tool lands on meshes",
        description: "Whether the procedural Paint tool's cursor ray may hit meshes.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "procgen.hit.paint.skip_generated",
        category: "Procedural tools",
        label: "Paint tool skips generated output",
        description: "Whether the procedural Paint tool ignores entities a generator made, so it never builds on its own output.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "procgen.hit.paint.physics_mask",
        category: "Procedural tools",
        label: "Paint tool layer mask",
        description: "Render layers (one bit per layer) a mesh must be on for the procedural Paint tool to land on it; 4294967295 is every layer.",
        kind: PrefKind::Int {
            min: 0,
            max: 4_294_967_295,
        },
        default: PrefDefault::Int(4_294_967_295),
    },
    PrefDef {
        key: "procgen.hit.box.terrain",
        category: "Procedural tools",
        label: "Box tool lands on terrain",
        description: "Whether the procedural Box tool's cursor ray may hit terrain (spec 67 § 5.3).",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "procgen.hit.box.meshes",
        category: "Procedural tools",
        label: "Box tool lands on meshes",
        description: "Whether the procedural Box tool's cursor ray may hit meshes.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "procgen.hit.box.skip_generated",
        category: "Procedural tools",
        label: "Box tool skips generated output",
        description: "Whether the procedural Box tool ignores entities a generator made, so it never builds on its own output.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "procgen.hit.box.physics_mask",
        category: "Procedural tools",
        label: "Box tool layer mask",
        description: "Render layers (one bit per layer) a mesh must be on for the procedural Box tool to land on it; 4294967295 is every layer.",
        kind: PrefKind::Int {
            min: 0,
            max: 4_294_967_295,
        },
        default: PrefDefault::Int(4_294_967_295),
    },
    // The separate play window (`crate::play_window`, ADR-7272): written when the window closes.
    PrefDef {
        key: "play.window_width",
        category: "Play",
        label: "Play window width",
        description: "Width of the separate play window in physical pixels (remembered from the last one).",
        kind: PrefKind::Int { min: 160, max: 16384 },
        default: PrefDefault::Int(1280),
    },
    PrefDef {
        key: "play.window_height",
        category: "Play",
        label: "Play window height",
        description: "Height of the separate play window in physical pixels (remembered from the last one).",
        kind: PrefKind::Int { min: 120, max: 16384 },
        default: PrefDefault::Int(720),
    },
    PrefDef {
        key: "play.window_placed",
        category: "Play",
        label: "Remember play window position",
        description: "Open the play window where the last one was; off lets the window system place it (Wayland always does).",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(false),
    },
    PrefDef {
        key: "play.window_x",
        category: "Play",
        label: "Play window left edge",
        description: "Left edge of the play window on the desktop, physical pixels (used when the position is remembered).",
        kind: PrefKind::Int {
            min: -65536,
            max: 65536,
        },
        default: PrefDefault::Int(0),
    },
    PrefDef {
        key: "play.window_y",
        category: "Play",
        label: "Play window top edge",
        description: "Top edge of the play window on the desktop, physical pixels (used when the position is remembered).",
        kind: PrefKind::Int {
            min: -65536,
            max: 65536,
        },
        default: PrefDefault::Int(0),
    },
    // The play window's chosen resolution (`crate::play_window::PlayResolution`, ADR-7283).
    PrefDef {
        key: "play.resolution",
        category: "Play",
        label: "Play resolution",
        description: "Size the play world renders at in the play window, scaled to fit with bars: the window's own size, a preset (720p, 1080p, phone 9:16 or 9:19.5, tablet 3:4) or custom.",
        kind: PrefKind::Choice(crate::play_window::RESOLUTION_CHOICES),
        default: PrefDefault::Text("window"),
    },
    PrefDef {
        key: "play.resolution_width",
        category: "Play",
        label: "Custom play width",
        description: "Width of the custom play resolution, pixels (used when the play resolution is custom).",
        kind: PrefKind::Int { min: 16, max: 16384 },
        default: PrefDefault::Int(1280),
    },
    PrefDef {
        key: "play.resolution_height",
        category: "Play",
        label: "Custom play height",
        description: "Height of the custom play resolution, pixels (used when the play resolution is custom).",
        kind: PrefKind::Int { min: 16, max: 16384 },
        default: PrefDefault::Int(720),
    },
    // Viewport navigation (ADR-7755): the session defaults of `camera_nav::CameraNav` and the clip planes.
    PrefDef {
        key: "camera.invert_orbit_x",
        category: "Camera",
        label: "Invert orbit X",
        description: "Horizontal orbit drags turn the camera the other way.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(false),
    },
    PrefDef {
        key: "camera.invert_orbit_y",
        category: "Camera",
        label: "Invert orbit Y",
        description: "Vertical orbit drags tilt the camera the other way.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(false),
    },
    PrefDef {
        key: "camera.invert_pan_x",
        category: "Camera",
        label: "Invert pan X",
        description: "Horizontal pan drags move the view the other way.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(false),
    },
    PrefDef {
        key: "camera.invert_pan_y",
        category: "Camera",
        label: "Invert pan Y",
        description: "Vertical pan drags move the view the other way.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(false),
    },
    PrefDef {
        key: "camera.invert_fly_y",
        category: "Camera",
        label: "Invert fly look Y",
        description: "Vertical look drags while flying (secondary button) tilt the other way.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(false),
    },
    PrefDef {
        key: "camera.invert_wheel",
        category: "Camera",
        label: "Invert wheel",
        description: "The wheel zooms (and changes the fly speed) the other way.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(false),
    },
    PrefDef {
        key: "camera.speed_with_distance",
        category: "Camera",
        label: "Speed scales with distance",
        description: "The fly speed grows with the distance to the orbit target (the set speed at 8 m).",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(false),
    },
    PrefDef {
        key: "camera.orbit_selection",
        category: "Camera",
        label: "Orbit around selection",
        description: "Orbit drags turn around the selection's center without framing it first (the target when nothing is selected).",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(false),
    },
    PrefDef {
        key: "camera.zoom_to_cursor",
        category: "Camera",
        label: "Zoom toward cursor",
        description: "The wheel zooms toward the point under the pointer instead of the view center.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "camera.navigation_preset",
        category: "Camera",
        label: "Navigation preset",
        description: "Mouse chords and view keys: Regia default, three-button mouse emulation (Alt+Shift+left pans), or numpad emulation (the number row as a keypad for views).",
        kind: PrefKind::Choice(crate::camera_nav::NAV_PRESET_NAMES),
        default: PrefDefault::Text("regia_default"),
    },
    PrefDef {
        key: "camera.near_clip",
        category: "Camera",
        label: "Near clip plane",
        description: "The perspective viewport's near clip plane in meters when a session starts (each pane can change it; far stays at least ten times near).",
        kind: PrefKind::Float {
            min: 0.001,
            max: 100.0,
        },
        default: PrefDefault::Float(0.1),
    },
    PrefDef {
        key: "camera.far_clip",
        category: "Camera",
        label: "Far clip plane",
        description: "The perspective viewport's far clip plane in meters when a session starts (each pane can change it).",
        kind: PrefKind::Float {
            min: 1.0,
            max: 100000.0,
        },
        default: PrefDefault::Float(4000.0),
    },
    // The selected camera's picture-in-picture preview size (`crate::camera_preview`, ADR-7768).
    PrefDef {
        key: "viewport.camera_preview",
        category: "Viewport",
        label: "Camera preview",
        description: "Size of the selected scene camera's picture-in-picture preview in the viewport: off, small, medium or large.",
        kind: PrefKind::Choice(crate::camera_preview::SIZE_CHOICES),
        default: PrefDefault::Text("medium"),
    },
    PrefDef {
        key: "inspector.linked_vectors",
        category: "Inspector",
        label: "Linked vector fields",
        description: "Vector fields whose components the inspector edits together (the chain toggle beside the field), as Component.field separated by commas: editing one axis scales the others by the same ratio.",
        kind: PrefKind::Text { max: 1024 },
        default: PrefDefault::Text(""),
    },
    PrefDef {
        key: "color.swatches",
        category: "Color picker",
        label: "Saved swatches",
        description: "The color picker's saved swatches: #rrggbbaa colors separated by commas (Save swatch in any color field adds one).",
        kind: PrefKind::Text { max: 32 * 10 },
        default: PrefDefault::Text(""),
    },
    PrefDef {
        key: "color.recent",
        category: "Color picker",
        label: "Recent colors",
        description: "The color picker's recent colors, newest first: #rrggbbaa colors separated by commas.",
        kind: PrefKind::Text { max: 12 * 10 },
        default: PrefDefault::Text(""),
    },
    PrefDef {
        key: "vcs.git_command",
        category: "Revision control",
        label: "Git program",
        description: "The git program the Revision Control window runs (a path or a name on PATH). Empty uses git from PATH. Agents cannot change it.",
        kind: PrefKind::Text { max: 1024 },
        default: PrefDefault::Text(""),
    },
    PrefDef {
        key: "review.author",
        category: "Review",
        label: "Review author",
        description: "Review note author when git user.name is unavailable.",
        kind: PrefKind::Text { max: 256 },
        default: PrefDefault::Text("Reviewer"),
    },
    // Floated panels as OS windows or inside the main window (ADR-7821).
    PrefDef {
        key: crate::os_windows::FLOATING_WINDOWS_PREF,
        category: "Appearance",
        label: "Floating windows",
        description: "Where a floated panel goes: os gives it its own window (it can move to another monitor); in_window keeps every panel inside the editor's one window.",
        kind: PrefKind::Choice(crate::os_windows::FLOAT_MODE_CHOICES),
        default: PrefDefault::Text(crate::os_windows::DEFAULT_FLOAT_MODE),
    },
    // The play-mode tint (`crate::pie_controls::PREF_TINT`, ADR-7829).
    PrefDef {
        key: "play.tint",
        category: "Play",
        label: "Tint the editor while playing",
        description: "While a play session runs, border the play viewport and tint the play toolbar with the theme's play color (accents.play_mode; per network role with client worlds), so edits made in play are obvious.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(true),
    },
    PrefDef {
        key: "agents.commands",
        category: "Agents",
        label: "Agent commands",
        description: "JSON list of named commands: [{\"name\":\"Local\",\"command\":[\"agent\",\"--stdio\"]}]. No shell expansion. Keep credentials in the external agent's environment, never in this setting.",
        kind: PrefKind::Text { max: 16384 },
        default: PrefDefault::Text(""),
    },
    PrefDef {
        key: "vcs.lock_on_edit",
        category: "Revision control",
        label: "Lock on edit",
        description: "What the first edit of a lockable file (git LFS) does: ask whether to lock it, always lock it, or never lock it.",
        kind: PrefKind::Choice(crate::vcs::locks::LOCK_ON_EDIT_CHOICES),
        default: PrefDefault::Text("ask"),
    },
    PrefDef {
        key: "vcs.lockable_read_only",
        category: "Revision control",
        label: "Show lockable files read-only",
        description: "Show lockable files (git LFS) that you do not lock with a read-only badge in the Content Browser.",
        kind: PrefKind::Bool,
        default: PrefDefault::Bool(false),
    },
];

/// Preferences only the owner sets: `editor.prefs_set` refuses them. The code editor command names a
/// program the editor runs, and a request must never pick the program.
pub const OWNER_ONLY_PREFS: &[&str] = &["code.editor_command", "vcs.git_command", "agents.commands"];

/// The setting called `key`.
#[must_use]
pub fn find_pref(key: &str) -> Option<&'static PrefDef> {
    PREFS.iter().find(|p| p.key == key)
}

/// Why a preference edit or a file read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefsError {
    /// No setting has the key.
    UnknownPref(String),
    /// The value does not fit the setting.
    BadValue(String),
    /// The file is not valid RON of the expected shape.
    Parse(String),
    /// The file was written by a newer editor (its version is above [`EDITOR_PREFS_FORMAT_VERSION`]).
    Newer {
        /// The file's version.
        found: u32,
    },
    /// Reading or writing the file failed.
    Io(String),
    /// No setting has the category (`editor.prefs_reset { category }`).
    UnknownCategory(String),
}

impl PrefsError {
    /// The agent error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownPref(_) => "unknown_pref",
            Self::BadValue(_) => "bad_pref_value",
            Self::Parse(_) => "prefs_parse",
            Self::Newer { .. } => "prefs_newer",
            Self::Io(_) => "prefs_io",
            Self::UnknownCategory(_) => "unknown_category",
        }
    }
}

impl std::fmt::Display for PrefsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPref(key) => write!(f, "no preference is called {key:?}"),
            Self::UnknownCategory(name) => write!(
                f,
                "no preference category is called {name:?} (categories: {})",
                categories().join(", ")
            ),
            Self::BadValue(message) | Self::Parse(message) | Self::Io(message) => f.write_str(message),
            Self::Newer { found } => write!(
                f,
                "written by a newer editor (format_version {found}, this editor reads up to {EDITOR_PREFS_FORMAT_VERSION}); left untouched"
            ),
        }
    }
}

impl std::error::Error for PrefsError {}

/// The user's editor preferences (only the values that differ from the defaults).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct EditorPreferences {
    values: BTreeMap<&'static str, PrefValue>,
    /// Keys this build does not know, kept so a save does not drop a newer editor's settings.
    unknown: BTreeMap<String, PrefValue>,
    /// Bumped by every change ([`Self::generation`]).
    generation: u64,
}

impl EditorPreferences {
    /// The effective value of `key` (the default when unset). `None` for an unknown key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<PrefValue> {
        let def = find_pref(key)?;
        Some(
            self.values
                .get(def.key)
                .cloned()
                .unwrap_or_else(|| def.default.value()),
        )
    }

    /// A bool setting's value (`false` for an unknown key or another kind).
    #[must_use]
    pub fn bool(&self, key: &str) -> bool {
        matches!(self.get(key), Some(PrefValue::Bool(true)))
    }

    /// A float setting's value (0 for an unknown key or another kind).
    #[must_use]
    pub fn float(&self, key: &str) -> f64 {
        match self.get(key) {
            Some(PrefValue::Float(f)) => f,
            Some(PrefValue::Int(i)) => i as f64,
            _ => 0.0,
        }
    }

    /// An integer setting's value (0 for an unknown key or another kind).
    #[must_use]
    pub fn int(&self, key: &str) -> i64 {
        match self.get(key) {
            Some(PrefValue::Int(i)) => i,
            _ => 0,
        }
    }

    /// A choice setting's value (empty for an unknown key or another kind).
    #[must_use]
    pub fn text(&self, key: &str) -> String {
        match self.get(key) {
            Some(PrefValue::Text(t)) => t,
            _ => String::new(),
        }
    }

    /// Whether `key` holds its default.
    #[must_use]
    pub fn is_default(&self, key: &str) -> bool {
        !self.values.contains_key(key)
    }

    /// Sets `key` to `value` (a value equal to the default is stored as "unset"). Returns whether
    /// the effective value changed.
    ///
    /// # Errors
    ///
    /// [`PrefsError::UnknownPref`] or [`PrefsError::BadValue`].
    pub fn set(&mut self, key: &str, value: &PrefValue) -> Result<bool, PrefsError> {
        let def = find_pref(key).ok_or_else(|| PrefsError::UnknownPref(key.to_owned()))?;
        let value = def.check(value).map_err(PrefsError::BadValue)?;
        let before = self.get(def.key);
        if value == def.default.value() {
            self.values.remove(def.key);
        } else {
            self.values.insert(def.key, value);
        }
        let changed = before != self.get(def.key);
        if changed {
            self.generation += 1;
        }
        Ok(changed)
    }

    /// Returns `key` to its default; whether it was set.
    ///
    /// # Errors
    ///
    /// [`PrefsError::UnknownPref`].
    pub fn reset(&mut self, key: &str) -> Result<bool, PrefsError> {
        let def = find_pref(key).ok_or_else(|| PrefsError::UnknownPref(key.to_owned()))?;
        let removed = self.values.remove(def.key).is_some();
        if removed {
            self.generation += 1;
        }
        Ok(removed)
    }

    /// Returns every setting to its default (unknown keys kept); how many were set.
    pub fn reset_all(&mut self) -> usize {
        let n = self.values.len();
        self.values.clear();
        if n > 0 {
            self.generation += 1;
        }
        n
    }

    /// Returns every setting of `category` (its Preferences section, case-insensitive) to its
    /// default; answers the keys that were set.
    ///
    /// # Errors
    ///
    /// [`PrefsError::UnknownCategory`].
    pub fn reset_category(&mut self, category: &str) -> Result<Vec<&'static str>, PrefsError> {
        let defs: Vec<&'static PrefDef> = PREFS
            .iter()
            .filter(|d| d.category.eq_ignore_ascii_case(category))
            .collect();
        if defs.is_empty() {
            return Err(PrefsError::UnknownCategory(category.to_owned()));
        }
        let reset: Vec<&'static str> = defs
            .iter()
            .filter(|d| self.values.remove(d.key).is_some())
            .map(|d| d.key)
            .collect();
        if !reset.is_empty() {
            self.generation += 1;
        }
        Ok(reset)
    }

    /// Combines these preferences with the preferences file `text` (ADR-7767, Preferences ▸
    /// Import): `merge` sets the file's values over the current ones, `replace` keeps only the
    /// file's. Unless `allow_owner_only`, the [`OWNER_ONLY_PREFS`] keep their current value (an agent
    /// import cannot pick the program the editor runs). Invalid values are dropped with a
    /// diagnostic, as on load; the file's unknown keys are kept.
    ///
    /// # Errors
    ///
    /// [`PrefsError::Parse`] or [`PrefsError::Newer`].
    pub fn import(
        &self,
        text: &str,
        mode: crate::editor_keymap::ImportMode,
        allow_owner_only: bool,
    ) -> Result<PrefsImport, PrefsError> {
        let (file, diagnostics) = Self::from_text(text)?;
        let mut next = match mode {
            crate::editor_keymap::ImportMode::Merge => {
                let mut next = self.clone();
                next.values.extend(file.values.clone());
                // A file value equal to its default is not stored, so merge leaves that key as is.
                next.unknown.extend(file.unknown.clone());
                next
            }
            crate::editor_keymap::ImportMode::Replace => Self {
                values: file.values.clone(),
                unknown: file.unknown.clone(),
                generation: self.generation,
            },
        };
        let mut skipped = Vec::new();
        if !allow_owner_only {
            for key in OWNER_ONLY_PREFS {
                let mine = self.values.get(key).cloned();
                if next.values.get(key) != mine.as_ref() {
                    skipped.push((*key).to_owned());
                    match (find_pref(key), mine) {
                        (Some(def), Some(v)) => {
                            next.values.insert(def.key, v);
                        }
                        _ => {
                            next.values.remove(key);
                        }
                    }
                }
            }
        }
        let mut applied: Vec<String> = file
            .values
            .keys()
            .filter(|k| !skipped.iter().any(|s| s == *k))
            .map(|k| (*k).to_owned())
            .collect();
        applied.sort();
        let changed: Vec<String> = PREFS
            .iter()
            .filter(|d| self.get(d.key) != next.get(d.key))
            .map(|d| d.key.to_owned())
            .collect();
        if !changed.is_empty() {
            next.generation = self.generation + 1;
        }
        Ok(PrefsImport {
            prefs: next,
            applied,
            changed,
            skipped,
            diagnostics,
        })
    }

    /// A counter bumped by every change, for code that applies the settings.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The keys this build does not know, as read from the file.
    #[must_use]
    pub fn unknown_keys(&self) -> Vec<&str> {
        self.unknown.keys().map(String::as_str).collect()
    }

    /// A value kept under a key outside the registry: extension settings live here as
    /// `extension.<ext>.<name>` (ADR-7791), so they are saved with the file like a newer editor's
    /// keys.
    #[must_use]
    pub fn extra(&self, key: &str) -> Option<&PrefValue> {
        self.unknown.get(key)
    }

    /// Sets (`Some`) or removes (`None`) a value under a key outside the registry (see
    /// [`Self::extra`]). Returns whether it changed; a change bumps [`Self::generation`], so the
    /// file is saved.
    pub fn set_extra(&mut self, key: &str, value: Option<PrefValue>) -> bool {
        let changed = match value {
            Some(value) => self.unknown.insert(key.to_owned(), value.clone()) != Some(value),
            None => self.unknown.remove(key).is_some(),
        };
        if changed {
            self.generation += 1;
        }
        changed
    }

    /// The settings matching `query`, best first: a fuzzy match over `Category: Label`, then a
    /// plain substring of the key or description. An empty query lists all in registry order.
    #[must_use]
    pub fn search(query: &str) -> Vec<&'static PrefDef> {
        let query = query.trim();
        if query.is_empty() {
            return PREFS.iter().collect();
        }
        let lower = query.to_lowercase();
        let mut rows: Vec<(i32, usize, &'static PrefDef)> = PREFS
            .iter()
            .enumerate()
            .filter_map(|(i, def)| {
                let title = format!("{}: {}", def.category, def.label);
                if let Some((score, _)) = regia_ui::fuzzy::fuzzy_match(query, &title) {
                    return Some((score + 1000, i, def));
                }
                def.haystack()
                    .to_lowercase()
                    .contains(&lower)
                    .then_some((0, i, def))
            })
            .collect();
        rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        rows.into_iter().map(|(_, _, def)| def).collect()
    }

    /// Reads `editor_prefs.ron` text: migrates, validates and returns the preferences with the
    /// diagnostics of every value that was dropped.
    ///
    /// # Errors
    ///
    /// [`PrefsError::Parse`] for text that is not a preferences file, [`PrefsError::Newer`] for a
    /// file from a newer editor.
    pub fn from_text(text: &str) -> Result<(Self, Vec<String>), PrefsError> {
        let file = migrate_prefs(text)?;
        let mut diagnostics = Vec::new();
        let mut prefs = Self::default();
        for (key, value) in file.values {
            match find_pref(&key) {
                None => {
                    diagnostics.push(format!("{key}: unknown preference, kept as is"));
                    prefs.unknown.insert(key, value);
                }
                Some(def) => match def.check(&value) {
                    Ok(value) if value == def.default.value() => {}
                    Ok(value) => {
                        prefs.values.insert(def.key, value);
                    }
                    Err(message) => diagnostics.push(format!("{message}; using the default")),
                },
            }
        }
        Ok((prefs, diagnostics))
    }

    /// The file text (current version, keys sorted, unknown keys kept).
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut values: BTreeMap<String, PrefValue> = self.unknown.clone();
        for (key, value) in &self.values {
            values.insert((*key).to_owned(), value.clone());
        }
        let file = PrefsFileV1 {
            format_version: EDITOR_PREFS_FORMAT_VERSION,
            values,
        };
        ron::ser::to_string_pretty(&file, regia_core::ron_pretty())
            .unwrap_or_else(|_| format!("(format_version: {EDITOR_PREFS_FORMAT_VERSION}, values: {{}})"))
    }
}

/// What a preferences import did (or would do, for a dry run).
#[derive(Debug, Clone, PartialEq)]
pub struct PrefsImport {
    /// The preferences after the import.
    pub prefs: EditorPreferences,
    /// The keys the file set (non-default values), sorted.
    pub applied: Vec<String>,
    /// The keys whose effective value changed.
    pub changed: Vec<String>,
    /// Owner-only keys the import left alone.
    pub skipped: Vec<String>,
    /// Values the file reader dropped, and unknown keys.
    pub diagnostics: Vec<String>,
}

/// The Preferences window's categories, in registry order.
#[must_use]
pub fn categories() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for def in PREFS {
        if !out.contains(&def.category) {
            out.push(def.category);
        }
    }
    out
}

/// The file-name ending `editor.prefs_export` requires (`editor_prefs.ron`, `my.prefs.ron`).
pub const PREFS_EXPORT_SUFFIX: &str = "prefs.ron";

/// Version 1 of the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PrefsFileV1 {
    #[serde(default = "version_missing")]
    format_version: u32,
    #[serde(default)]
    values: BTreeMap<String, PrefValue>,
}

/// The version a file without `format_version` gets while probing (RON writes a plain `1`, which
/// does not read as an `Option`, so absence is a sentinel).
pub(crate) const VERSION_MISSING: u32 = u32::MAX;

/// The serde default of a `format_version` field: [`VERSION_MISSING`].
pub(crate) fn version_missing() -> u32 {
    VERSION_MISSING
}

/// Why [`read_versioned`] refused a file.
pub(crate) enum VersionedError {
    /// Not the expected shape.
    Parse(String),
    /// A newer version than this build reads.
    Newer(u32),
}

/// Reads a versioned editor file of the current shape: a missing `format_version` reads as the
/// current one, 0 is refused, a newer version is refused (whether or not its shape parses: when it
/// does not, the version alone is probed).
pub(crate) fn read_versioned<T: serde::de::DeserializeOwned>(
    text: &str,
    current: u32,
    version_of: fn(&T) -> u32,
) -> Result<T, VersionedError> {
    match ron::from_str::<T>(text) {
        Ok(value) => match version_of(&value) {
            VERSION_MISSING => Ok(value),
            0 => Err(VersionedError::Parse(
                "format_version 0 is not a valid version".to_owned(),
            )),
            found if found > current => Err(VersionedError::Newer(found)),
            _ => Ok(value),
        },
        Err(e) => match probe_version(text) {
            Ok(Some(found)) if found > current => Err(VersionedError::Newer(found)),
            _ => Err(VersionedError::Parse(e.to_string())),
        },
    }
}

/// Only the version field, read first to pick the migration path.
#[derive(Deserialize)]
struct VersionProbe {
    #[serde(default = "version_missing")]
    format_version: u32,
}

/// Reads the `format_version` of a versioned editor file (`None` when the field is missing).
///
/// # Errors
///
/// The text is not RON of a struct.
pub(crate) fn probe_version(text: &str) -> Result<Option<u32>, String> {
    ron::from_str::<VersionProbe>(text)
        .map(|probe| (probe.format_version != VERSION_MISSING).then_some(probe.format_version))
        .map_err(|e| e.to_string())
}

/// The `from` versions of the registered `editor_prefs.ron` migration steps (none: version 1 is
/// the first).
#[must_use]
pub fn prefs_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape of `editor_prefs.ron` (ADR-7902: the `editor-prefs` format).
#[must_use]
pub fn prefs_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<PrefsFileV1>(),
        regia_core::schema::serde_variant_names::<PrefValue>(),
        regia_core::schema::serde_variant_shapes::<PrefValue>(),
    ])
}

/// Migrates `editor_prefs.ron` text of any supported version to the current shape.
fn migrate_prefs(text: &str) -> Result<PrefsFileV1, PrefsError> {
    // Version 1 is the only shape (a file without the field reads as version 1); a step from an
    // older version goes here when the shape changes.
    let mut file = read_versioned(text, EDITOR_PREFS_FORMAT_VERSION, |f: &PrefsFileV1| {
        f.format_version
    })
    .map_err(|e| match e {
        VersionedError::Parse(m) => PrefsError::Parse(m),
        VersionedError::Newer(found) => PrefsError::Newer { found },
    })?;
    file.format_version = EDITOR_PREFS_FORMAT_VERSION;
    Ok(file)
}

/// The per-user editor configuration directory.
///
/// The base is resolved per OS so a Windows process launched from a POSIX shell (Git Bash,
/// MSYS, some CI launchers) does not inherit that shell's `HOME`/`XDG_CONFIG_HOME` and write
/// its config next to a different base than the same editor started from Explorer:
///
/// - `$REGIA_EDITOR_CONFIG_DIR` (and its pre-rename spelling) always wins.
/// - On Windows: `%APPDATA%`, then `%USERPROFILE%`, then the temp directory.
/// - Elsewhere: `$XDG_CONFIG_HOME`, then `$HOME/.config`, then `%APPDATA%`, then
///   `%USERPROFILE%`, then the temp directory (the last two keep stripped-environment
///   launches that set only one of them on the pre-change location, ADR-4825).
///
/// The chosen base gets a `regia` sub-directory.
#[must_use]
pub fn user_config_dir() -> PathBuf {
    user_config_dir_from(|key| regia_core::brand::env_var_os(key), cfg!(windows))
}

/// Pure resolution of [`user_config_dir`] over an `env` lookup closure (production passes
/// [`regia_core::brand::env_var_os`], which also reads the pre-rename spelling).
///
/// A variable that is set but empty counts as unset (the XDG rule): `PathBuf::from("")` would put
/// the files in a `regia` directory relative to wherever the editor was started.
pub(crate) fn user_config_dir_from(
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    windows: bool,
) -> PathBuf {
    let env = |key: &str| env(key).filter(|value| !value.is_empty());
    if let Some(dir) = env("REGIA_EDITOR_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let base = if windows {
        env("APPDATA")
            .map(PathBuf::from)
            .or_else(|| env("USERPROFILE").map(PathBuf::from))
    } else {
        env("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env("HOME").map(|home| PathBuf::from(home).join(".config")))
            .or_else(|| env("APPDATA").map(PathBuf::from))
            .or_else(|| env("USERPROFILE").map(PathBuf::from))
    }
    .unwrap_or_else(std::env::temp_dir);
    base.join("regia")
}

/// Where the per-user editor files live. Insert it to make the editor read and write them
/// (`None` keeps everything for the session).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct EditorConfigPaths {
    /// The directory holding `editor_prefs.ron`, `editor_keymap.ron` and `editor_layout.ron`.
    pub dir: Option<PathBuf>,
}

impl EditorConfigPaths {
    /// The real user directory ([`user_config_dir`]).
    #[must_use]
    pub fn user() -> Self {
        Self {
            dir: Some(user_config_dir()),
        }
    }

    /// A given directory (tests, portable installs).
    #[must_use]
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: Some(dir.into()),
        }
    }

    /// `editor_prefs.ron`.
    #[must_use]
    pub fn prefs_file(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(PREFS_FILE))
    }

    /// `editor_keymap.ron`.
    #[must_use]
    pub fn keymap_file(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(KEYMAP_FILE))
    }

    /// The user's default `editor_layout.ron`.
    #[must_use]
    pub fn user_layout_file(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(USER_LAYOUT_FILE))
    }
}

/// The persistence state of the preferences file.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct PrefsStore {
    /// Whether the file was read this session (or there is no file to read).
    pub loaded: bool,
    /// The generation last written (or read).
    pub saved_generation: u64,
    /// Set when the file must not be written (a newer editor wrote it); the reason.
    pub read_only: Option<String>,
    /// What the last read reported (dropped values, unknown keys).
    pub diagnostics: Vec<String>,
    /// The last write error, if any.
    pub last_error: Option<String>,
}

/// Reads the preferences file named by [`EditorConfigPaths`] into the world (once per session;
/// later calls do nothing). A missing file is not an error. Settings changed in this session before
/// the read (an agent call before the first frame) win over the file's and are saved.
pub fn load_prefs(world: &mut World) {
    if world.get_resource::<PrefsStore>().is_some_and(|s| s.loaded) {
        return;
    }
    let path = world
        .get_resource::<EditorConfigPaths>()
        .and_then(EditorConfigPaths::prefs_file);
    let mut store = PrefsStore {
        loaded: true,
        ..Default::default()
    };
    let mut prefs = EditorPreferences::default();
    if let Some(path) = path {
        match read_prefs_file(&path) {
            Ok(Some((read, diagnostics))) => {
                prefs = read;
                store.diagnostics = diagnostics;
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(target: "regia_editor", "{}: {err}", path.display());
                if matches!(err, PrefsError::Newer { .. }) {
                    store.read_only = Some(err.to_string());
                } else {
                    // A broken file is replaced by the next save; say so once.
                    store
                        .diagnostics
                        .push(format!("{}: {err}; using the defaults", path.display()));
                }
            }
        }
    }
    store.saved_generation = prefs.generation();
    if let Some(session) = world
        .get_resource::<EditorPreferences>()
        .filter(|p| p.generation() > 0)
        .cloned()
    {
        for def in PREFS {
            if let Some(value) = session.get(def.key).filter(|_| !session.is_default(def.key)) {
                let _ = prefs.set(def.key, &value);
            }
        }
    }
    world.insert_resource(prefs);
    world.insert_resource(store);
}

/// Reads a preferences file; `Ok(None)` when it does not exist.
///
/// # Errors
///
/// The file cannot be read, does not parse, or is from a newer editor.
pub fn read_prefs_file(path: &Path) -> Result<Option<(EditorPreferences, Vec<String>)>, PrefsError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(PrefsError::Io(format!("cannot read {}: {e}", path.display()))),
    };
    EditorPreferences::from_text(&text).map(Some)
}

/// Writes the preferences when they changed since the last write (and the file is writable).
/// Returns whether it wrote.
pub fn persist_prefs(world: &mut World) -> bool {
    let Some(path) = world
        .get_resource::<EditorConfigPaths>()
        .and_then(EditorConfigPaths::prefs_file)
    else {
        return false;
    };
    let Some(prefs) = world.get_resource::<EditorPreferences>() else {
        return false;
    };
    let generation = prefs.generation();
    let store = world.get_resource::<PrefsStore>().cloned().unwrap_or_default();
    if store.saved_generation == generation || store.read_only.is_some() {
        return false;
    }
    let text = prefs.to_text();
    let result = crate::level_io::write_atomic(&path, text.as_bytes());
    let mut store = store;
    match result {
        Ok(()) => {
            store.saved_generation = generation;
            store.last_error = None;
        }
        Err(err) => {
            tracing::warn!(target: "regia_editor", "preferences not saved: {err}");
            // Do not retry every frame: the next change tries again.
            store.saved_generation = generation;
            store.last_error = Some(err);
        }
    }
    let wrote = store.last_error.is_none();
    world.insert_resource(store);
    wrote
}

/// What the last [`apply_prefs`] applied: the effective value of every setting.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct AppliedPrefs {
    generation: Option<u64>,
    values: BTreeMap<&'static str, PrefValue>,
}

/// Applies changed preferences to the editor: theme (with the custom theme and diagnostic colors,
/// [`apply_appearance`]), UI scale and text scale (`ctx`), camera speed and field of view, viewport
/// grid and realtime. The first call applies only the settings that are not at
/// their default, so an editor without preferences is left exactly as it was; later calls apply
/// the settings whose value changed. Returns the keys it applied.
pub fn apply_prefs(ctx: Option<&egui::Context>, world: &mut World) -> Vec<&'static str> {
    // ADR-7733: egui's keyboard zoom is off (its Ctrl+0 reset clashed with camera bookmark 0);
    // the View zoom commands change `appearance.ui_scale` instead, which is saved.
    if let Some(ctx) = ctx
        && ctx.options(|o| o.zoom_with_keyboard)
    {
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
    }
    let Some(prefs) = world.get_resource::<EditorPreferences>().cloned() else {
        return Vec::new();
    };
    let mut applied = world.get_resource::<AppliedPrefs>().cloned().unwrap_or_default();
    if applied.generation == Some(prefs.generation()) {
        return Vec::new();
    }
    let first = applied.generation.is_none();
    let mut changed = Vec::new();
    let mut appearance = false;
    let mut theme_reload = false;
    for def in PREFS {
        let value = prefs.get(def.key).unwrap_or_else(|| def.default.value());
        let differs = if first {
            !prefs.is_default(def.key)
        } else {
            applied.values.get(def.key) != Some(&value)
        };
        applied.values.insert(def.key, value.clone());
        if differs {
            if is_appearance_color_key(def.key) {
                appearance = true;
                theme_reload |= !DIAGNOSTIC_COLOR_PREFS.contains(&def.key);
            } else {
                apply_one(ctx, world, def.key, &value);
            }
            changed.push(def.key);
        }
    }
    applied.generation = Some(prefs.generation());
    world.insert_resource(applied);
    // A theme choice reloads the theme file; a diagnostic color alone is reapplied over the tokens
    // in the world, so an unsaved Theme page edit survives (review sprint-w12-rev-g14 #3).
    if appearance {
        if theme_reload || !world.contains_resource::<AppearanceBase>() {
            apply_appearance(world);
        } else {
            reapply_diagnostic_colors(world);
        }
    }
    changed
}

/// The theme tokens [`apply_appearance`] loaded, before the diagnostic color overrides.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct AppearanceBase(pub crate::theme::ThemeTokens);

/// Reapplies the diagnostic color preferences over the world's tokens: their slots go back to the
/// loaded theme ([`AppearanceBase`]), then the overrides apply; every other token is left alone.
pub fn reapply_diagnostic_colors(world: &mut World) {
    let Some(base) = world.get_resource::<AppearanceBase>().cloned() else {
        apply_appearance(world);
        return;
    };
    let prefs = world
        .get_resource::<EditorPreferences>()
        .cloned()
        .unwrap_or_default();
    let mut tokens = world
        .get_resource::<crate::theme::ThemeTokens>()
        .cloned()
        .unwrap_or_else(|| base.0.clone());
    crate::theme::DiagnosticColors::restore_slots(&base.0, &mut tokens);
    diagnostic_colors(&prefs).apply_to(&mut tokens);
    world.insert_resource(tokens);
}

/// A step of the interface zoom (View ▸ Zoom in / Zoom out / Reset zoom, ADR-7733).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoomStep {
    /// One [`ZOOM_STEP`] larger.
    In,
    /// One [`ZOOM_STEP`] smaller.
    Out,
    /// Back to 100 %.
    Reset,
}

/// How much one zoom key changes `appearance.ui_scale`.
pub const ZOOM_STEP: f64 = 0.1;

/// Changes `appearance.ui_scale` by `step` (clamped to the preference's range, rounded to the
/// step) and returns the new scale. The change goes through [`EditorPreferences::set`], so
/// [`apply_prefs`] applies it to the interface and [`persist_prefs`] saves it.
pub fn zoom_ui_scale(world: &mut World, step: ZoomStep) -> f64 {
    const KEY: &str = "appearance.ui_scale";
    let mut prefs = world
        .get_resource_or_insert_with(EditorPreferences::default)
        .clone();
    let now = prefs.float(KEY);
    let (min, max) = match find_pref(KEY).map(|def| def.kind) {
        Some(PrefKind::Float { min, max }) => (min, max),
        _ => (0.5, 3.0),
    };
    let to = match step {
        ZoomStep::In => now + ZOOM_STEP,
        ZoomStep::Out => now - ZOOM_STEP,
        ZoomStep::Reset => 1.0,
    };
    // Tenths (ZOOM_STEP), dividing last so 1.1 saves as 1.1, not 1.1000000000000001.
    let to = ((to * 10.0).round() / 10.0).clamp(min, max);
    if let Err(err) = prefs.set(KEY, &PrefValue::Float(to)) {
        tracing::warn!(target: "regia_editor", "ui_scale: {err}");
        return now;
    }
    world.insert_resource(prefs);
    to
}

/// Whether the last [`apply_appearance`] could not load the custom theme, and why.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct AppearanceStatus {
    /// The custom theme problem (missing file, bad file), shown on the Theme page.
    pub custom_theme_error: Option<String>,
}

/// The diagnostic color overrides of `prefs` (empty values keep the theme's).
#[must_use]
pub fn diagnostic_colors(prefs: &EditorPreferences) -> crate::theme::DiagnosticColors {
    let color = |key: &str| {
        let text = prefs.text(key);
        crate::theme::parse_color(&serde_json::Value::String(text)).ok()
    };
    crate::theme::DiagnosticColors {
        error: color(DIAGNOSTIC_COLOR_PREFS[0]),
        warning: color(DIAGNOSTIC_COLOR_PREFS[1]),
        info: color(DIAGNOSTIC_COLOR_PREFS[2]),
        success: color(DIAGNOSTIC_COLOR_PREFS[3]),
    }
}

/// Composes the editor theme from the preferences and installs it (tokens and source): the Theme
/// choice, replaced by the saved custom theme when `appearance.custom_theme` names one that loads
/// (from [`EditorConfigPaths`]), then the diagnostic color overrides. A custom theme that does not
/// load keeps the Theme choice and is reported in [`AppearanceStatus`].
pub fn apply_appearance(world: &mut World) {
    use crate::ui_roles::ThemeSource;
    let prefs = world
        .get_resource::<EditorPreferences>()
        .cloned()
        .unwrap_or_default();
    let choice = prefs.text("appearance.theme");
    let (mut tokens, mut source) = match crate::theme::builtin_editor_theme(&choice) {
        Ok(found) => found,
        Err(err) => {
            tracing::warn!(target: "regia_editor", "theme preference {choice}: {err}");
            (crate::theme::ThemeTokens::regia_default(), ThemeSource::Default)
        }
    };
    let mut status = AppearanceStatus::default();
    let custom = prefs.text(CUSTOM_THEME_PREF);
    if !custom.is_empty() {
        let dir = world
            .get_resource::<EditorConfigPaths>()
            .and_then(|p| p.dir.clone());
        let loaded = match dir {
            Some(dir) => crate::theme::load_custom_theme(&dir, &custom).map_err(|e| e.to_string()),
            None => Err("no user config directory: saved themes are off for this session".to_owned()),
        };
        match loaded {
            Ok(saved) => {
                tokens = saved;
                source = ThemeSource::Ui(format!("custom:{custom}"));
            }
            Err(err) => {
                tracing::warn!(target: "regia_editor", "custom theme {custom}: {err}");
                status.custom_theme_error = Some(format!("{custom}: {err}"));
            }
        }
    }
    world.insert_resource(AppearanceBase(tokens.clone()));
    diagnostic_colors(&prefs).apply_to(&mut tokens);
    world.insert_resource(tokens);
    world.insert_resource(source);
    world.insert_resource(status);
}

/// Id of the unscaled text styles kept in an egui context by [`apply_text_scale`].
fn text_base_key() -> egui::Id {
    egui::Id::new("regia_editor::editor_prefs::text_base")
}

/// Scales every egui text style of `ctx` to `scale` times its size before the first call (kept in
/// the context), separately from the zoom factor of `appearance.ui_scale`.
pub fn apply_text_scale(ctx: &egui::Context, scale: f32) {
    type Base = std::sync::Arc<BTreeMap<egui::TextStyle, f32>>;
    // Never read the style inside `data_mut`: both lock the context.
    let stored: Option<Base> = ctx.data(|d| d.get_temp(text_base_key()));
    let base = match stored {
        Some(base) => base,
        None => {
            let sizes: BTreeMap<egui::TextStyle, f32> = ctx
                .global_style()
                .text_styles
                .iter()
                .map(|(style, font)| (style.clone(), font.size))
                .collect();
            let base: Base = std::sync::Arc::new(sizes);
            ctx.data_mut(|d| d.insert_temp(text_base_key(), base.clone()));
            base
        }
    };
    let scale = if scale.is_finite() {
        scale.clamp(0.5, 4.0)
    } else {
        1.0
    };
    ctx.all_styles_mut(|style| {
        for (text_style, font) in &mut style.text_styles {
            if let Some(size) = base.get(text_style) {
                font.size = size * scale;
            }
        }
    });
}

/// Applies one setting.
fn apply_one(ctx: Option<&egui::Context>, world: &mut World, key: &str, value: &PrefValue) {
    match (key, value) {
        (TEXT_SCALE_PREF, PrefValue::Float(scale)) => {
            if let Some(ctx) = ctx {
                apply_text_scale(ctx, *scale as f32);
            }
        }
        ("appearance.ui_scale", PrefValue::Float(scale)) => {
            if let Some(ctx) = ctx {
                ctx.set_zoom_factor(*scale as f32);
            }
        }
        ("camera.fly_speed", PrefValue::Float(speed)) => {
            world
                .get_resource_or_insert_with(crate::viewport::EditorCamera::default)
                .speed = *speed as f32;
        }
        ("camera.field_of_view", PrefValue::Float(fov)) => {
            world
                .get_resource_or_insert_with(crate::viewport::EditorCamera::default)
                .fov_deg = *fov as f32;
        }
        // Viewport navigation (ADR-7755).
        (key, value) if key.starts_with("camera.") && crate::camera_nav::apply_pref(world, key, value) => {}
        ("viewport.grid", PrefValue::Bool(on)) => {
            world
                .get_resource_or_insert_with(crate::viewport::ViewportSettings::default)
                .grid = *on;
        }
        ("viewport.realtime", PrefValue::Bool(on)) => {
            world
                .get_resource_or_insert_with(crate::viewport::ViewportSettings::default)
                .realtime = *on;
        }
        // The camera preview size (ADR-7768).
        ("viewport.camera_preview", PrefValue::Text(name)) => {
            if let Some(size) = crate::camera_preview::PreviewSize::from_name(name) {
                world
                    .get_resource_or_insert_with(crate::camera_preview::CameraPreview::default)
                    .size = size;
            }
        }
        // Autosave and layout settings are read where they act (the shell).
        _ => {}
    }
}

/// Sizes the root window's next egui pass: the screen rectangle in points for a surface of
/// `size_in_pixels` on a display of `native_pixels_per_point` (the window's scale factor), and the
/// root viewport's native scale (ADR-8589). The UI-scale preference is egui's zoom factor
/// ([`apply_prefs`] sets it once per change), so the pass draws at native scale x
/// `appearance.ui_scale` pixels per point.
pub fn frame_screen(
    ctx: &egui::Context,
    raw: &mut egui::RawInput,
    native_pixels_per_point: f32,
    size_in_pixels: [u32; 2],
) {
    let native = if native_pixels_per_point.is_finite() && native_pixels_per_point > 0.0 {
        native_pixels_per_point
    } else {
        1.0
    };
    raw.viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .native_pixels_per_point = Some(native);
    // egui's pixels per point is native x zoom. Setting it here (as the runner did every frame)
    // would turn the zoom back to 1 and undo the preference. A zoom set this frame applies at the
    // start of the next pass, and egui rescales that pass's screen rectangle itself.
    let pixels_per_point = native * ctx.zoom_factor();
    raw.screen_rect = Some(egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(
            size_in_pixels[0] as f32 / pixels_per_point,
            size_in_pixels[1] as f32 / pixels_per_point,
        ),
    ));
}

/// The autosave interval the preferences ask for (`None` = autosave off).
#[must_use]
pub fn autosave_interval(prefs: &EditorPreferences) -> Option<std::time::Duration> {
    let minutes = prefs.int("autosave.interval_minutes");
    (minutes > 0).then(|| std::time::Duration::from_secs(minutes.unsigned_abs() * 60))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Review sprint-w12-rev-g14 #3: an unsaved Theme page edit (in the world's tokens) survives a
    /// diagnostic color change; clearing the color restores the theme's slot; a theme choice
    /// reloads the file.
    #[test]
    fn a_diagnostic_color_change_keeps_unsaved_theme_edits() {
        let mut world = World::new();
        world.insert_resource(EditorPreferences::default());
        apply_appearance(&mut world);
        apply_prefs(None, &mut world);
        let base = world.resource::<crate::theme::ThemeTokens>().clone();
        let edited = egui::Color32::from_rgb(1, 2, 3);
        world.resource_mut::<crate::theme::ThemeTokens>().signal = edited;
        let red = egui::Color32::from_rgb(250, 10, 10);
        world
            .resource_mut::<EditorPreferences>()
            .set("appearance.color_error", &PrefValue::Text("#fa0a0a".into()))
            .unwrap();
        apply_prefs(None, &mut world);
        let tokens = world.resource::<crate::theme::ThemeTokens>().clone();
        assert_eq!(tokens.signal, edited, "the unsaved edit survives");
        assert_eq!(tokens.error, red);
        world
            .resource_mut::<EditorPreferences>()
            .set("appearance.color_error", &PrefValue::Text(String::new()))
            .unwrap();
        apply_prefs(None, &mut world);
        let tokens = world.resource::<crate::theme::ThemeTokens>().clone();
        assert_eq!(tokens.error, base.error, "cleared: the theme's color again");
        assert_eq!(tokens.signal, edited);
    }

    /// FU-4 (ADR-7733): Ctrl+Plus / Ctrl+= / Ctrl+Minus step `appearance.ui_scale`, the zoom
    /// reset has its own rebindable binding (Ctrl+Alt+0 built in), egui's own keyboard zoom is
    /// off, and the scale the keys set survives a save and load of the preferences.
    #[test]
    fn zoom_keys_change_the_saved_ui_scale_and_reset_has_its_own_binding() {
        use egui::{Key, Modifiers};
        let ctx = egui::Context::default();
        let mut world = World::new();
        world.insert_resource(EditorPreferences::default());
        let key = |key: Key, modifiers: Modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let frame = |world: &mut World, events: Vec<egui::Event>| {
            apply_prefs(Some(&ctx), world);
            let raw = egui::RawInput {
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 300.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                let _ = crate::command_palette::show(ui.ctx(), world, false);
            });
            out.textures_delta.clear();
            apply_prefs(Some(&ctx), world);
        };
        let scale = |world: &World| world.resource::<EditorPreferences>().float("appearance.ui_scale");
        frame(&mut world, vec![]);
        assert!(
            !ctx.options(|o| o.zoom_with_keyboard),
            "egui's keyboard zoom is off"
        );
        frame(&mut world, vec![key(Key::Plus, Modifiers::COMMAND)]);
        frame(&mut world, vec![key(Key::Equals, Modifiers::COMMAND)]);
        assert_eq!(scale(&world), 1.2, "two zoom-in presses");
        frame(&mut world, vec![key(Key::Minus, Modifiers::COMMAND)]);
        assert_eq!(scale(&world), 1.1);
        frame(&mut world, vec![key(Key::Num0, Modifiers::COMMAND)]);
        assert_eq!(scale(&world), 1.1, "Ctrl+0 is not the zoom reset");
        frame(&mut world, vec![]);
        assert!(
            (ctx.zoom_factor() - 1.1).abs() < 1e-6,
            "applied: {}",
            ctx.zoom_factor()
        );
        // Saved and read back.
        let text = world.resource::<EditorPreferences>().to_text();
        let (read, unknown) = EditorPreferences::from_text(&text).expect("read back");
        assert!(unknown.is_empty());
        assert_eq!(read.float("appearance.ui_scale"), 1.1, "{text}");
        assert!(text.contains("1.1"), "{text}");
        // The reset's built-in binding, then a user binding that replaces it.
        frame(
            &mut world,
            vec![key(Key::Num0, Modifiers::COMMAND | Modifiers::ALT)],
        );
        assert_eq!(scale(&world), 1.0, "Ctrl+Alt+0 resets");
        frame(&mut world, vec![key(Key::Plus, Modifiers::COMMAND)]);
        let chord = crate::command_palette::KeyChord::parse("Ctrl+Shift+0").expect("chord");
        world
            .get_resource_or_insert_with(crate::command_palette::CommandKeymap::default)
            .bind("view.reset_zoom", chord)
            .expect("bind");
        frame(
            &mut world,
            vec![key(Key::Num0, Modifiers::COMMAND | Modifiers::ALT)],
        );
        assert_eq!(
            scale(&world),
            1.1,
            "the rebound reset no longer answers Ctrl+Alt+0"
        );
        frame(
            &mut world,
            vec![key(Key::Num0, Modifiers::COMMAND | Modifiers::SHIFT)],
        );
        assert_eq!(scale(&world), 1.0, "the user's binding resets");
        // Clamped to the preference's range.
        for _ in 0..40 {
            zoom_ui_scale(&mut world, ZoomStep::In);
        }
        assert_eq!(scale(&world), 3.0);
    }

    /// Bug 5 of the 2026-11-16 all-host smoke run (ADR-8589): the windowed editor reset egui's
    /// zoom to the window's scale factor every frame, so `appearance.ui_scale` changed nothing on
    /// screen. Driving passes the way the runner does ([`frame_screen`] then `run_ui`), a fixed
    /// label at 200 % covers twice the physical pixels it covers at 100 %, on a 1x and a 1.5x
    /// display, and the egui screen still covers the whole surface.
    #[test]
    fn ui_scale_200_doubles_a_label_on_screen_at_every_native_scale() {
        const SURFACE: [u32; 2] = [1600, 1000];
        let measure = |native: f32, scale: f64| -> (egui::Vec2, f32, egui::Vec2) {
            let ctx = egui::Context::default();
            let mut world = World::new();
            world.insert_resource(EditorPreferences::default());
            world
                .resource_mut::<EditorPreferences>()
                .set("appearance.ui_scale", &PrefValue::Float(scale))
                .unwrap();
            let mut label = egui::Rect::NOTHING;
            let mut ppp = 0.0;
            let mut screen = egui::Vec2::ZERO;
            // A few passes: a zoom set by the preference takes effect at the next pass.
            for _ in 0..4 {
                apply_prefs(Some(&ctx), &mut world);
                let mut raw = egui::RawInput::default();
                frame_screen(&ctx, &mut raw, native, SURFACE);
                let mut out = ctx.run_ui(raw, |ui| {
                    label = ui.label("UI scale probe: Fixed label").rect;
                });
                out.textures_delta.clear();
                ppp = out.pixels_per_point;
                screen = ctx.content_rect().size() * ppp;
            }
            (label.size() * ppp, ppp, screen)
        };
        for native in [1.0_f32, 1.5] {
            let (at_100, ppp_100, screen_100) = measure(native, 1.0);
            let (at_200, ppp_200, screen_200) = measure(native, 2.0);
            let ratio = at_200 / at_100;
            assert!(
                (ratio.x - 2.0).abs() < 0.05 && (ratio.y - 2.0).abs() < 0.05,
                "native {native}: the label covers {at_100:?} px at 100 % and {at_200:?} px at 200 % \
                 (pixels per point {ppp_100} and {ppp_200})"
            );
            assert!(
                (ppp_200 - 2.0 * native).abs() < 1e-4,
                "native {native}: {ppp_200}"
            );
            for screen in [screen_100, screen_200] {
                assert!(
                    (screen.x - SURFACE[0] as f32).abs() < 1.0 && (screen.y - SURFACE[1] as f32).abs() < 1.0,
                    "native {native}: egui's screen covers {screen:?} px of the {SURFACE:?} surface"
                );
            }
        }
    }

    #[test]
    fn registry_keys_are_unique_and_defaults_fit_their_kind() {
        let mut seen = std::collections::HashSet::new();
        for def in PREFS {
            assert!(seen.insert(def.key), "duplicate {}", def.key);
            assert_eq!(
                def.check(&def.default.value()),
                Ok(def.default.value()),
                "{}",
                def.key
            );
            assert!(def.key.contains('.'), "{} is not dotted", def.key);
        }
    }

    #[test]
    fn defaults_match_the_editor_before_preferences() {
        let camera = crate::viewport::EditorCamera::default();
        let viewport = crate::viewport::ViewportSettings::default();
        let prefs = EditorPreferences::default();
        assert_eq!(prefs.float("camera.fly_speed") as f32, camera.speed);
        assert_eq!(prefs.float("camera.field_of_view") as f32, camera.fov_deg);
        assert_eq!(prefs.bool("viewport.grid"), viewport.grid);
        assert_eq!(prefs.bool("viewport.realtime"), viewport.realtime);
        assert_eq!(
            autosave_interval(&prefs),
            Some(crate::autosave::AUTOSAVE_INTERVAL)
        );
        assert_eq!(prefs.text("appearance.theme"), "default");
        assert_eq!(prefs.float("appearance.ui_scale"), 1.0);
    }

    fn env_map<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| std::ffi::OsString::from(v))
        }
    }

    #[test]
    fn user_config_dir_keeps_the_stripped_env_fallbacks_elsewhere() {
        // A stripped-environment launch that sets only APPDATA (some CI/headless harnesses do)
        // keeps the pre-ADR-4825 location instead of silently falling through to the temp
        // directory, where prefs would be ephemeral.
        let env = env_map(&[("APPDATA", "/odd/appdata")]);
        assert_eq!(
            user_config_dir_from(env, false),
            PathBuf::from("/odd/appdata/regia")
        );
        let env = env_map(&[("USERPROFILE", "/odd/userprofile")]);
        assert_eq!(
            user_config_dir_from(env, false),
            PathBuf::from("/odd/userprofile/regia")
        );
        // HOME still beats both fallbacks.
        let env = env_map(&[("HOME", "/home/dev"), ("APPDATA", "/odd/appdata")]);
        assert_eq!(
            user_config_dir_from(env, false),
            PathBuf::from("/home/dev/.config/regia")
        );
    }

    #[test]
    fn user_config_dir_prefers_appdata_over_home_on_windows() {
        // Git Bash / MSYS set HOME (and sometimes XDG_CONFIG_HOME) for child processes; the
        // Windows editor must still use APPDATA so a Explorer/cmd-launched instance reads the
        // same files.
        let env = env_map(&[
            ("APPDATA", r"C:\Users\dev\AppData\Roaming"),
            ("HOME", r"C:\Users\dev"),
            ("XDG_CONFIG_HOME", r"C:\Users\dev\.config"),
        ]);
        assert_eq!(
            user_config_dir_from(env, true),
            PathBuf::from(r"C:\Users\dev\AppData\Roaming").join("regia")
        );
        // No APPDATA: fall back to USERPROFILE, never to HOME.
        let env = env_map(&[("HOME", r"C:\Users\dev"), ("USERPROFILE", r"C:\Users\dev")]);
        assert_eq!(
            user_config_dir_from(env, true),
            PathBuf::from(r"C:\Users\dev").join("regia")
        );
    }

    #[test]
    fn user_config_dir_keeps_the_xdg_order_elsewhere() {
        let env = env_map(&[
            ("XDG_CONFIG_HOME", "/home/dev/.xdg"),
            ("HOME", "/home/dev"),
            ("APPDATA", "/odd/appdata"),
        ]);
        assert_eq!(
            user_config_dir_from(env, false),
            PathBuf::from("/home/dev/.xdg/regia")
        );
        let env = env_map(&[("HOME", "/home/dev"), ("APPDATA", "/odd/appdata")]);
        assert_eq!(
            user_config_dir_from(env, false),
            PathBuf::from("/home/dev/.config/regia")
        );
        // The explicit override wins everywhere.
        let env = env_map(&[
            ("REGIA_EDITOR_CONFIG_DIR", "/override"),
            ("XDG_CONFIG_HOME", "/home/dev/.xdg"),
        ]);
        assert_eq!(user_config_dir_from(env, false), PathBuf::from("/override"));
    }

    /// Corr editor-a finding 7: an empty variable counts as unset. `XDG_CONFIG_HOME=` used to
    /// resolve to the relative `regia`, so the prefs moved with the working directory.
    #[test]
    fn user_config_dir_skips_empty_variables() {
        let env = env_map(&[
            ("REGIA_EDITOR_CONFIG_DIR", ""),
            ("XDG_CONFIG_HOME", ""),
            ("HOME", "/home/dev"),
        ]);
        assert_eq!(
            user_config_dir_from(env, false),
            PathBuf::from("/home/dev/.config/regia")
        );
        let env = env_map(&[("APPDATA", ""), ("USERPROFILE", "C:/Users/dev")]);
        assert_eq!(
            user_config_dir_from(env, true),
            PathBuf::from("C:/Users/dev").join("regia")
        );
    }

    #[test]
    fn category_reset_returns_only_that_category() {
        let mut prefs = EditorPreferences::default();
        prefs
            .set("appearance.ui_scale", &PrefValue::Float(1.5))
            .expect("scale");
        prefs
            .set("camera.fly_speed", &PrefValue::Float(20.0))
            .expect("fly");
        let g = prefs.generation();
        assert_eq!(
            prefs.reset_category("appearance").expect("known"),
            ["appearance.ui_scale"]
        );
        assert!(prefs.generation() > g);
        assert!(prefs.is_default("appearance.ui_scale"));
        assert!(
            !prefs.is_default("camera.fly_speed"),
            "other categories keep their values"
        );
        assert_eq!(
            prefs.reset_category("Appearance").expect("again"),
            Vec::<&str>::new()
        );
        assert_eq!(
            prefs.reset_category("nope").expect_err("unknown").code(),
            "unknown_category"
        );
        assert!(categories().contains(&"Camera"));
    }

    #[test]
    fn import_merges_or_replaces_and_keeps_owner_only_keys() {
        use crate::editor_keymap::ImportMode;
        let mut current = EditorPreferences::default();
        current
            .set("camera.fly_speed", &PrefValue::Float(20.0))
            .expect("fly");
        let mut other = EditorPreferences::default();
        other
            .set("appearance.ui_scale", &PrefValue::Float(1.25))
            .expect("scale");
        other
            .set("code.editor_command", &PrefValue::Text("evil {file}".into()))
            .expect("owner");
        let text = other.to_text();
        let merged = current.import(&text, ImportMode::Merge, false).expect("merge");
        assert_eq!(merged.prefs.float("appearance.ui_scale"), 1.25);
        assert_eq!(
            merged.prefs.float("camera.fly_speed"),
            20.0,
            "merge keeps current values"
        );
        assert_eq!(merged.skipped, ["code.editor_command"]);
        assert!(merged.prefs.is_default("code.editor_command"));
        assert!(merged.prefs.generation() > current.generation());
        let replaced = current.import(&text, ImportMode::Replace, true).expect("replace");
        assert!(
            replaced.prefs.is_default("camera.fly_speed"),
            "replace drops current values"
        );
        assert_eq!(replaced.prefs.text("code.editor_command"), "evil {file}");
        assert!(replaced.skipped.is_empty());
        // Export then import round-trips.
        let back = EditorPreferences::default()
            .import(&replaced.prefs.to_text(), ImportMode::Replace, true)
            .expect("round trip");
        assert_eq!(back.prefs.to_text(), replaced.prefs.to_text());
        assert_eq!(
            current
                .import("not ron", ImportMode::Merge, false)
                .expect_err("parse")
                .code(),
            "prefs_parse"
        );
    }
}
